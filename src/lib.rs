use std::{fs::File, io::Read, path::PathBuf, process::Child};

use error::LaunchError;
use getset::Getters;
use network::ActivationHeights;
use portpicker::Port;
use tempfile::TempDir;

pub(crate) mod config;
pub mod error;
pub mod network;
pub(crate) mod utils;

const STDOUT_LOG: &str = "stdout.log";

/// Struct associated with Zcashd process.
#[derive(Getters)]
#[getset(get = "pub")]
pub struct Zcashd {
    handle: Child,
    port: Port,
    _data_dir: TempDir,
    logs_dir: TempDir,
    config_dir: TempDir,
    zcash_cli_bin: Option<PathBuf>,
}

impl Zcashd {
    /// Launches Zcashd process and returns [`crate::Zcashd`] with the handle and associated directories.
    ///
    /// Use `zcashd_bin` and `zcash_cli_bin` to specify the paths to the binaries.
    /// If these binaries are in $PATH, `None` can be specified to run "zcashd" / "zcash-cli".
    ///
    /// Use `fixed_port` to specify a port for Zcashd. Otherwise, a port is picked at random.
    ///
    /// Use `activation_heights` to specify custom network upgrade activation heights
    ///
    /// Use `miner_address` to specify the target address for the block rewards when blocks are generated.  
    pub fn launch(
        zcashd_bin: Option<PathBuf>,
        zcash_cli_bin: Option<PathBuf>,
        rpc_port: Option<Port>,
        activation_heights: &ActivationHeights,
        miner_address: Option<&str>,
    ) -> Result<Zcashd, LaunchError> {
        let port = utils::pick_unused_port(rpc_port);
        let config_dir = tempfile::tempdir().unwrap();
        let config_file_path =
            config::zcashd(config_dir.path(), port, activation_heights, miner_address).unwrap();

        let data_dir = tempfile::tempdir().unwrap();

        let mut command = match zcashd_bin {
            Some(path) => std::process::Command::new(path),
            None => std::process::Command::new("zcashd"),
        };
        command
            .args([
                "--printtoconsole",
                format!(
                    "--conf={}",
                    config_file_path.to_str().expect("should be valid UTF-8")
                )
                .as_str(),
                format!(
                    "--datadir={}",
                    data_dir.path().to_str().expect("should be valid UTF-8")
                )
                .as_str(),
                "-debug=1",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut handle = command.spawn().unwrap();

        let logs_dir = tempfile::tempdir().unwrap();
        let stdout_log_path = logs_dir.path().join(STDOUT_LOG);
        let mut stdout_log = File::create(&stdout_log_path).unwrap();
        let mut stdout = handle.stdout.take().unwrap();
        // TODO: consider writing logs in a runtime to increase performance
        std::thread::spawn(move || {
            std::io::copy(&mut stdout, &mut stdout_log)
                .expect("should be able to read/write stdout log");
        });

        let mut stdout_log = File::open(stdout_log_path).expect("should be able to open log");
        let mut stdout = String::new();

        let check_interval = std::time::Duration::from_millis(100);

        // wait for stdout log entry that indicates daemon is ready
        loop {
            match handle.try_wait() {
                Ok(Some(exit_status)) => {
                    stdout_log.read_to_string(&mut stdout).unwrap();

                    let mut stderr = String::new();
                    handle
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut stderr)
                        .unwrap();

                    return Err(LaunchError::ProcessFailed {
                        process_name: "zcashd".to_string(),
                        exit_status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => (),
                Err(e) => {
                    panic!("Unexpected Error: {e}")
                }
            };

            stdout_log.read_to_string(&mut stdout).unwrap();
            if stdout.contains("Error:") {
                panic!("Zcashd launch failed without reporting an error code!\nexiting with panic. you may have to shut the daemon down manually.");
            } else if stdout.contains("init message: Done loading") {
                // launch successful
                break;
            }

            std::thread::sleep(check_interval);
        }

        Ok(Zcashd {
            handle,
            port,
            _data_dir: data_dir,
            logs_dir,
            config_dir,
            zcash_cli_bin,
        })
    }

    /// Returns path to config file.
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.path().join(config::ZCASHD_FILENAME)
    }

    /// Runs a Zcash-cli command with the given `args`.
    ///
    /// Example usage for generating blocks in Zcashd local net:
    /// ```ignore (incomplete)
    /// self.zcash_cli_command(&["generate", "1"]);
    /// ```
    pub fn zcash_cli_command(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut command = match &self.zcash_cli_bin {
            Some(path) => std::process::Command::new(path),
            None => std::process::Command::new("zcash-cli"),
        };

        command.arg(format!("-conf={}", self.config_path().to_str().unwrap()));
        command.args(args).output()
    }

    /// Stops the Zcashd process.
    pub fn stop(&mut self) {
        match self.zcash_cli_command(&["stop"]) {
            Ok(_) => {
                if let Err(e) = self.handle.wait() {
                    tracing::error!("zcashd cannot be awaited: {e}")
                } else {
                    tracing::info!("zcashd successfully shut down")
                };
            }
            Err(e) => {
                tracing::error!(
                    "Can't stop zcashd from zcash-cli: {e}\n\
                    Sending SIGKILL to zcashd process."
                );
                if let Err(e) = self.handle.kill() {
                    tracing::warn!("zcashd has already terminated: {e}")
                };
            }
        }
    }

    /// Generate `num_blocks` blocks.
    pub fn generate_blocks(&self, num_blocks: u32) -> std::io::Result<std::process::Output> {
        self.zcash_cli_command(&["generate", &num_blocks.to_string()])
    }

    /// Prints the stdout log.
    pub fn print_stdout(&self) {
        let stdout_log_path = self.logs_dir.path().join(STDOUT_LOG);
        let mut stdout_log = File::open(stdout_log_path).expect("should be able to open log");
        let mut stdout = String::new();
        stdout_log.read_to_string(&mut stdout).unwrap();
        println!("{}", stdout);
    }
}

impl Default for Zcashd {
    /// Default launch for Zcashd.
    /// Panics on failure.
    fn default() -> Self {
        Zcashd::launch(None, None, None, &ActivationHeights::default(), None).unwrap()
    }
}

impl Drop for Zcashd {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Struct associated with Zcashd process.
#[derive(Getters)]
#[getset(get = "pub")]
pub struct Zainod {
    handle: Child,
    port: Port,
    logs_dir: TempDir,
    config_dir: TempDir,
}

impl Zainod {
    /// Launches Zainod process and returns [`crate::Zainod`] with the handle and associated directories.
    ///
    /// Use `fixed_port` to specify a port for Zainod. Otherwise, a port is picked at random.
    ///
    /// The `validator_port` must be specified and the validator process must be running before launching Zainod.
    pub fn launch(
        zainod_bin: Option<PathBuf>,
        listen_port: Option<Port>,
        validator_port: Port,
    ) -> Result<Zainod, LaunchError> {
        let port = utils::pick_unused_port(listen_port);
        let config_dir = tempfile::tempdir().unwrap();
        let config_file_path = config::zainod(config_dir.path(), port, validator_port).unwrap();

        let mut command = match zainod_bin {
            Some(path) => std::process::Command::new(path),
            None => std::process::Command::new("zainod"),
        };
        command
            .args([
                "--config",
                format!(
                    "{}",
                    config_file_path.to_str().expect("should be valid UTF-8")
                )
                .as_str(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut handle = command.spawn().unwrap();

        let logs_dir = tempfile::tempdir().unwrap();
        let stdout_log_path = logs_dir.path().join(STDOUT_LOG);
        let mut stdout_log = File::create(&stdout_log_path).unwrap();
        let mut stdout = handle.stdout.take().unwrap();
        // TODO: consider writing logs in a runtime to increase performance
        std::thread::spawn(move || {
            std::io::copy(&mut stdout, &mut stdout_log)
                .expect("should be able to read/write stdout log");
        });

        let mut stdout_log = File::open(stdout_log_path).expect("should be able to open log");
        let mut stdout = String::new();

        let check_interval = std::time::Duration::from_millis(100);

        // wait for stdout log entry that indicates daemon is ready
        loop {
            match handle.try_wait() {
                Ok(Some(exit_status)) => {
                    stdout_log.read_to_string(&mut stdout).unwrap();

                    let mut stderr = String::new();
                    handle
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut stderr)
                        .unwrap();

                    return Err(LaunchError::ProcessFailed {
                        process_name: "zainod".to_string(),
                        exit_status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => (),
                Err(e) => {
                    panic!("Unexpected Error: {e}")
                }
            };

            stdout_log.read_to_string(&mut stdout).unwrap();
            if stdout.contains("Error:") {
                panic!("Zainod launch failed without reporting an error code!\nexiting with panic. you may have to shut the daemon down manually.");
            } else if stdout.contains("Server Ready.") {
                // launch successful
                break;
            }

            std::thread::sleep(check_interval);
        }

        Ok(Zainod {
            handle,
            port,
            logs_dir,
            config_dir,
        })
    }

    /// Returns path to config file.
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.path().join(config::ZAINOD_FILENAME)
    }

    /// Stops the Zcashd process.
    pub fn stop(&mut self) {
        self.handle.kill().expect("zainod couldn't be killed")
    }

    /// Prints the stdout log.
    pub fn print_stdout(&self) {
        let stdout_log_path = self.logs_dir.path().join(STDOUT_LOG);
        let mut stdout_log = File::open(stdout_log_path).expect("should be able to open log");
        let mut stdout = String::new();
        stdout_log.read_to_string(&mut stdout).unwrap();
        println!("{}", stdout);
    }
}

impl Default for Zainod {
    /// Default launch for Zainod.
    /// Panics on failure.
    fn default() -> Self {
        Zainod::launch(None, None, 18232).unwrap()
    }
}

impl Drop for Zainod {
    fn drop(&mut self) {
        self.stop();
    }
}
