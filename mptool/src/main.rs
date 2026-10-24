mod compare;
mod config;
mod menu;
mod monitor;
mod notify;
mod remote;
mod retention;
mod run;
mod screen;
mod status;
mod unpack;

use config::Config;
use std::process::ExitCode;

const USAGE: &str = "Usage:
  mptool               open the menu
  mptool run           run the backup now (the nightly timer calls this)
  mptool status        show the last backup and the nights on disk
  mptool monitor       check every customer's login page (the 15-minute timer calls this)
  mptool screen        show the logo and the status on the Pi's little screen
  mptool test-discord  send a test message to the Discord channel";

fn main() -> ExitCode {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e:#}");
            eprintln!("Example config: mptool/config.example.toml");
            return ExitCode::FAILURE;
        }
    };
    match std::env::args().nth(1).as_deref() {
        None => match menu::run(cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        Some("run") => match run::run(&cfg, &run::print_event) {
            Ok(st) => {
                if let Err(e) = notify::report(&cfg, &st) {
                    eprintln!("{e:#}");
                }
                if st.ok {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                eprintln!("Backup stopped: {e:#}");
                let _ = notify::send(&cfg, &format!("**Minipass backup FAILED**\n- {e:#}"));
                ExitCode::FAILURE
            }
        },
        Some("status") => {
            status::print(&cfg.root());
            ExitCode::SUCCESS
        }
        Some("monitor") => match monitor::run(&cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        Some("screen") => match screen::run(&cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        Some("test-discord") => match notify::send(&cfg, "Minipass backup: test message, alerts work.") {
            Ok(()) if cfg.discord_webhook.is_empty() => {
                eprintln!("No discord_webhook in the config.");
                ExitCode::FAILURE
            }
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
