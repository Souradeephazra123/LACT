use super::{
    gpu_controller::{fan_control::FanCurve, GpuController},
    system::PP_FEATURE_MASK_PATH,
};
use crate::config::{self, default_fan_static_speed, Config, FanControlSettings};
use anyhow::{anyhow, Context};
use lact_schema::{
    amdgpu_sysfs::{
        gpu_handle::{
            power_profile_mode::PowerProfileModesTable, PerformanceLevel, PowerLevelKind,
        },
        sysfs::SysFS,
    },
    default_fan_curve,
    request::{ConfirmCommand, SetClocksCommand},
    ClocksInfo, DeviceInfo, DeviceListEntry, DeviceStats, FanControlMode, FanCurveMap, PowerStates,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    env,
    fs::{File, Permissions},
    io::{BufWriter, Cursor, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
use tokio::{sync::oneshot, time::sleep};
use tracing::{debug, error, info, trace, warn};

const CONTROLLERS_LOAD_RETRY_ATTEMPTS: u8 = 5;
const CONTROLLERS_LOAD_RETRY_INTERVAL: u64 = 1;

const SNAPSHOT_GLOBAL_FILES: &[&str] = &[
    PP_FEATURE_MASK_PATH,
    "/etc/lact/config.yaml",
    "/proc/version",
];
const SNAPSHOT_DEVICE_FILES: &[&str] = &[
    "uevent",
    "vendor",
    "pp_cur_state",
    "pp_dpm_mclk",
    "pp_dpm_pcie",
    "pp_dpm_sclk",
    "pp_dpm_socclk",
    "pp_features",
    "pp_force_state",
    "pp_mclk_od",
    "pp_num_states",
    "pp_od_clk_voltage",
    "pp_power_profile_mode",
    "pp_sclk_od",
    "pp_table",
    "vbios_version",
    "gpu_busy_percent",
    "current_link_speed",
    "current_link_width",
];
const SNAPSHOT_HWMON_FILE_PREFIXES: &[&str] = &["fan", "pwm", "power", "temp", "freq", "in"];

#[derive(Clone)]
pub struct Handler {
    pub config: Rc<RefCell<Config>>,
    pub gpu_controllers: Rc<BTreeMap<String, GpuController>>,
    confirm_config_tx: Rc<RefCell<Option<oneshot::Sender<ConfirmCommand>>>>,
}

impl<'a> Handler {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let mut controllers = BTreeMap::new();

        // Sometimes LACT starts too early in the boot process, before the sysfs is initialized.
        // For such scenarios there is a retry logic when no GPUs were found
        for i in 1..=CONTROLLERS_LOAD_RETRY_ATTEMPTS {
            controllers = load_controllers()?;

            if controllers.is_empty() {
                warn!("no GPUs were found, retrying in {CONTROLLERS_LOAD_RETRY_INTERVAL}s (attempt {i}/{CONTROLLERS_LOAD_RETRY_ATTEMPTS})");
                sleep(Duration::from_secs(CONTROLLERS_LOAD_RETRY_INTERVAL)).await;
            } else {
                break;
            }
        }
        info!("initialized {} GPUs", controllers.len());

        let handler = Self {
            gpu_controllers: Rc::new(controllers),
            config: Rc::new(RefCell::new(config)),
            confirm_config_tx: Rc::new(RefCell::new(None)),
        };
        handler.load_config().await;

        Ok(handler)
    }

    pub async fn load_config(&self) {
        let config = self.config.borrow().clone(); // Clone to avoid locking the RwLock on an await point

        for (id, gpu_config) in &config.gpus {
            if let Some(controller) = self.gpu_controllers.get(id) {
                if let Err(err) = controller.apply_config(gpu_config).await {
                    error!("could not apply existing config for gpu {id}: {err}");
                }
            } else {
                info!("could not find GPU with id {id} defined in configuration");
            }
        }
    }

    async fn edit_gpu_config<F: FnOnce(&mut config::Gpu)>(
        &self,
        id: String,
        f: F,
    ) -> anyhow::Result<u64> {
        if self
            .confirm_config_tx
            .try_borrow_mut()
            .map_err(|err| anyhow!("{err}"))?
            .is_some()
        {
            return Err(anyhow!(
                "There is an unconfirmed configuration change pending"
            ));
        }

        let (gpu_config, apply_timer) = {
            let config = self.config.try_borrow().map_err(|err| anyhow!("{err}"))?;
            let apply_timer = config.apply_settings_timer;
            let gpu_config = config.gpus.get(&id).cloned().unwrap_or_default();
            (gpu_config, apply_timer)
        };

        let mut new_config = gpu_config.clone();
        f(&mut new_config);

        let controller = self.controller_by_id(&id)?;

        match controller.apply_config(&new_config).await {
            Ok(()) => {
                self.wait_config_confirm(id, gpu_config, new_config, apply_timer)?;
                Ok(apply_timer)
            }
            Err(apply_err) => {
                error!("could not apply settings: {apply_err:?}");
                match controller.apply_config(&gpu_config).await {
                        Ok(()) => Err(apply_err.context("Could not apply settings")),
                        Err(err) => Err(anyhow!("Could not apply settings, and could not reset to default settings: {err:?}")),
                    }
            }
        }
    }

    /// Should be called after applying new config without writing it
    fn wait_config_confirm(
        &self,
        id: String,
        previous_config: config::Gpu,
        new_config: config::Gpu,
        apply_timer: u64,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        *self
            .confirm_config_tx
            .try_borrow_mut()
            .map_err(|err| anyhow!("{err}"))? = Some(tx);

        let handler = self.clone();

        tokio::task::spawn_local(async move {
            let controller = handler
                .controller_by_id(&id)
                .expect("GPU controller disappeared");

            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(apply_timer)) => {
                    info!("no confirmation received, reverting settings");

                    if let Err(err) = controller.apply_config(&previous_config).await {
                        error!("could not revert settings: {err:#}");
                    }
                }
                result = rx => {
                    match result {
                        Ok(ConfirmCommand::Confirm) => {
                            info!("saving updated config");

                            let mut config_guard = handler.config.borrow_mut();
                            config_guard.gpus.insert(id, new_config);

                            if let Err(err) = config_guard.save() {
                                error!("{err}");
                            }
                        }
                        Ok(ConfirmCommand::Revert) | Err(_) => {
                            if let Err(err) = controller.apply_config(&previous_config).await {
                                error!("could not revert settings: {err:#}");
                            }
                        }
                    }
                }
            }

            match handler.confirm_config_tx.try_borrow_mut() {
                Ok(mut guard) => *guard = None,
                Err(err) => error!("{err}"),
            }
        });

        Ok(())
    }

    fn controller_by_id(&self, id: &str) -> anyhow::Result<&GpuController> {
        Ok(self
            .gpu_controllers
            .get(id)
            .as_ref()
            .context("No controller with such id")?)
    }

    pub fn list_devices(&'a self) -> Vec<DeviceListEntry<'a>> {
        self.gpu_controllers
            .iter()
            .map(|(id, controller)| {
                let name = controller
                    .pci_info
                    .as_ref()
                    .and_then(|pci_info| pci_info.device_pci_info.model.as_deref());
                DeviceListEntry { id, name }
            })
            .collect()
    }

    pub fn get_device_info(&'a self, id: &str) -> anyhow::Result<DeviceInfo<'a>> {
        Ok(self.controller_by_id(id)?.get_info())
    }

    pub fn get_gpu_stats(&'a self, id: &str) -> anyhow::Result<DeviceStats> {
        let config = self
            .config
            .try_borrow()
            .map_err(|err| anyhow!("Could not read config: {err:?}"))?;
        let gpu_config = config.gpus.get(id);
        Ok(self.controller_by_id(id)?.get_stats(gpu_config))
    }

    pub fn get_clocks_info(&'a self, id: &str) -> anyhow::Result<ClocksInfo> {
        self.controller_by_id(id)?.get_clocks_info()
    }

    pub async fn set_fan_control(
        &'a self,
        id: &str,
        enabled: bool,
        mode: Option<FanControlMode>,
        static_speed: Option<f64>,
        curve: Option<FanCurveMap>,
    ) -> anyhow::Result<u64> {
        let settings = {
            let mut config_guard = self
                .config
                .try_borrow_mut()
                .map_err(|err| anyhow!("{err}"))?;
            let gpu_config = config_guard.gpus.entry(id.to_owned()).or_default();

            match mode {
                Some(mode) => match mode {
                    FanControlMode::Static => {
                        if matches!(static_speed, Some(speed) if !(0.0..=1.0).contains(&speed)) {
                            return Err(anyhow!("static speed value out of range"));
                        }

                        if let Some(mut existing_settings) = gpu_config.fan_control_settings.clone()
                        {
                            existing_settings.mode = mode;
                            if let Some(static_speed) = static_speed {
                                existing_settings.static_speed = static_speed;
                            }
                            Some(existing_settings)
                        } else {
                            Some(FanControlSettings {
                                mode,
                                static_speed: static_speed.unwrap_or_else(default_fan_static_speed),
                                ..Default::default()
                            })
                        }
                    }
                    FanControlMode::Curve => {
                        if let Some(mut existing_settings) = gpu_config.fan_control_settings.clone()
                        {
                            existing_settings.mode = mode;
                            if let Some(raw_curve) = curve {
                                let curve = FanCurve(raw_curve);
                                curve.validate()?;
                                existing_settings.curve = curve;
                            }
                            Some(existing_settings)
                        } else {
                            let curve = FanCurve(curve.unwrap_or_else(default_fan_curve));
                            curve.validate()?;
                            Some(FanControlSettings {
                                mode,
                                curve,
                                ..Default::default()
                            })
                        }
                    }
                },
                None => None,
            }
        };

        self.edit_gpu_config(id.to_owned(), |config| {
            config.fan_control_enabled = enabled;
            if let Some(settings) = settings {
                config.fan_control_settings = Some(settings);
            }
        })
        .await
    }

    pub async fn set_power_cap(&'a self, id: &str, maybe_cap: Option<f64>) -> anyhow::Result<u64> {
        self.edit_gpu_config(id.to_owned(), |gpu_config| {
            gpu_config.power_cap = maybe_cap;
        })
        .await
    }

    pub fn get_power_states(&self, id: &str) -> anyhow::Result<PowerStates> {
        let config = self
            .config
            .try_borrow()
            .map_err(|err| anyhow!("Could not read config: {err:?}"))?;
        let gpu_config = config.gpus.get(id);

        let states = self.controller_by_id(id)?.get_power_states(gpu_config);
        Ok(states)
    }

    pub async fn set_performance_level(
        &self,
        id: &str,
        level: PerformanceLevel,
    ) -> anyhow::Result<u64> {
        self.edit_gpu_config(id.to_owned(), |gpu_config| {
            gpu_config.performance_level = Some(level);

            if level != PerformanceLevel::Manual {
                gpu_config.power_states.clear();
            }
        })
        .await
    }

    pub async fn set_clocks_value(
        &self,
        id: &str,
        command: SetClocksCommand,
    ) -> anyhow::Result<u64> {
        if let SetClocksCommand::Reset = command {
            self.controller_by_id(id)?.handle.reset_clocks_table()?;
        }

        self.edit_gpu_config(id.to_owned(), |gpu_config| {
            gpu_config.apply_clocks_command(&command);
        })
        .await
    }

    pub async fn batch_set_clocks_value(
        &self,
        id: &str,
        commands: Vec<SetClocksCommand>,
    ) -> anyhow::Result<u64> {
        self.edit_gpu_config(id.to_owned(), |gpu_config| {
            for command in commands {
                gpu_config.apply_clocks_command(&command);
            }
        })
        .await
    }

    pub fn get_power_profile_modes(&self, id: &str) -> anyhow::Result<PowerProfileModesTable> {
        let modes_table = self
            .controller_by_id(id)?
            .handle
            .get_power_profile_modes()?;
        Ok(modes_table)
    }

    pub async fn set_power_profile_mode(
        &self,
        id: &str,
        index: Option<u16>,
    ) -> anyhow::Result<u64> {
        self.edit_gpu_config(id.to_owned(), |gpu_config| {
            gpu_config.power_profile_mode_index = index;
        })
        .await
    }

    pub async fn set_enabled_power_states(
        &self,
        id: &str,
        kind: PowerLevelKind,
        enabled_states: Vec<u8>,
    ) -> anyhow::Result<u64> {
        self.edit_gpu_config(id.to_owned(), |gpu| {
            gpu.power_states.insert(kind, enabled_states);
        })
        .await
    }

    pub fn generate_snapshot(&self) -> anyhow::Result<String> {
        let datetime = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let out_path = format!("/tmp/LACT-sysfs-snapshot-{datetime}.tar");

        let out_file = File::create(&out_path)
            .with_context(|| "Could not create output file at {out_path}")?;
        let out_writer = BufWriter::new(out_file);

        let mut archive = tar::Builder::new(out_writer);

        for path in SNAPSHOT_GLOBAL_FILES {
            let path = Path::new(path);
            add_path_to_archive(&mut archive, path)?;
        }

        for controller in self.gpu_controllers.values() {
            let controller_path = controller.handle.get_path();

            for device_file in SNAPSHOT_DEVICE_FILES {
                let full_path = controller_path.join(device_file);
                add_path_to_archive(&mut archive, &full_path)?;
            }

            for hw_mon in &controller.handle.hw_monitors {
                let hw_mon_path = hw_mon.get_path();
                let hw_mon_entries =
                    std::fs::read_dir(hw_mon_path).context("Could not read HwMon dir")?;

                'entries: for entry in hw_mon_entries.flatten() {
                    if !entry.metadata().is_ok_and(|metadata| metadata.is_file()) {
                        continue;
                    }

                    if let Some(name) = entry.file_name().to_str() {
                        for prefix in SNAPSHOT_HWMON_FILE_PREFIXES {
                            if name.starts_with(prefix) {
                                add_path_to_archive(&mut archive, &entry.path())?;
                                continue 'entries;
                            }
                        }
                    }
                }
            }
        }

        let mut writer = archive.into_inner().context("Could not finish archive")?;
        writer.flush().context("Could not flush output file")?;

        writer
            .into_inner()?
            .set_permissions(Permissions::from_mode(0o775))
            .context("Could not set permissions on output file")?;

        Ok(out_path)
    }

    pub fn confirm_pending_config(&self, command: ConfirmCommand) -> anyhow::Result<()> {
        if let Some(tx) = self
            .confirm_config_tx
            .try_borrow_mut()
            .map_err(|err| anyhow!("{err}"))?
            .take()
        {
            tx.send(command)
                .map_err(|_| anyhow!("Could not confirm config"))
        } else {
            Err(anyhow!("No pending config changes"))
        }
    }

    pub async fn cleanup(self) {
        let disable_clocks_cleanup = self
            .config
            .try_borrow()
            .map(|config| config.daemon.disable_clocks_cleanup)
            .unwrap_or(false);

        for (id, controller) in &*self.gpu_controllers {
            if !disable_clocks_cleanup && controller.handle.get_clocks_table().is_ok() {
                debug!("resetting clocks table");
                if let Err(err) = controller.handle.reset_clocks_table() {
                    error!("could not reset the clocks table: {err}");
                }
            }

            if let Err(err) = controller.apply_config(&config::Gpu::default()).await {
                error!("Could not reset settings for controller {id}: {err:#}");
            }
        }
    }
}

fn load_controllers() -> anyhow::Result<BTreeMap<String, GpuController>> {
    let mut controllers = BTreeMap::new();

    let base_path = match env::var("_LACT_DRM_SYSFS_PATH") {
        Ok(custom_path) => PathBuf::from(custom_path),
        Err(_) => PathBuf::from("/sys/class/drm"),
    };

    for entry in base_path
        .read_dir()
        .map_err(|error| anyhow!("Failed to read sysfs: {error}"))?
    {
        let entry = entry?;

        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("non-utf path"))?;
        if name.starts_with("card") && !name.contains('-') {
            trace!("trying gpu controller at {:?}", entry.path());
            let device_path = entry.path().join("device");
            match GpuController::new_from_path(device_path) {
                Ok(controller) => match controller.get_id() {
                    Ok(id) => {
                        let path = controller.get_path();
                        debug!("initialized GPU controller {id} for path {path:?}",);
                        controllers.insert(id, controller);
                    }
                    Err(err) => warn!("could not initialize controller: {err:#}"),
                },
                Err(error) => {
                    warn!(
                        "failed to initialize controller at {:?}, {error}",
                        entry.path()
                    );
                }
            }
        }
    }

    Ok(controllers)
}

fn add_path_to_archive(
    archive: &mut tar::Builder<impl Write>,
    full_path: &Path,
) -> anyhow::Result<()> {
    let archive_path = full_path
        .strip_prefix("/")
        .context("Path should always start at root")?;

    if let Ok(metadata) = std::fs::metadata(full_path) {
        debug!("adding {full_path:?} to snapshot");
        let data = std::fs::read(full_path)
            .with_context(|| format!("Could not read file at {full_path:?}"))?;

        let mut header = tar::Header::new_gnu();
        header.set_size(data.len().try_into().unwrap());
        header.set_mode(metadata.mode());
        header.set_uid(metadata.uid().into());
        header.set_gid(metadata.gid().into());
        header.set_cksum();

        archive
            .append_data(&mut header, archive_path, Cursor::new(data))
            .context("Could not write data to archive")?;
    }
    Ok(())
}
