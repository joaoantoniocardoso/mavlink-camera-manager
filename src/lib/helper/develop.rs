use std::collections::HashMap;

use anyhow::{anyhow, Result};
use thirtyfour::prelude::*;
use tokio::{process::Command, runtime::Runtime, sync::RwLock};
use tracing::*;

use crate::{cli, helper};

pub struct ChromeWebDriver {
    _process: tokio::task::JoinHandle<()>,
    webdriver: WebDriver,
}

impl Drop for ChromeWebDriver {
    fn drop(&mut self) {
        self._process.abort();
    }
}

impl std::ops::Deref for ChromeWebDriver {
    type Target = WebDriver;

    fn deref(&self) -> &Self::Target {
        &self.webdriver
    }
}

impl ChromeWebDriver {
    #[instrument]
    pub async fn new() -> Result<Self> {
        let port = cli::manager::enable_webrtc_task_test().unwrap();

        let _process = tokio::spawn(async move {
            let res = Command::new("chromedriver")
                .args([
                    format!("--port={port}").as_str(),
                    "--allow-running-insecure-content",
                    "--autoplay-policy=user-gesture-required",
                    "--disable-add-to-shelf",
                    "--disable-background-networking",
                    "--disable-background-timer-throttling",
                    "--disable-backgrounding-occluded-windows",
                    "--disable-breakpad",
                    "--disable-checker-imaging",
                    "--disable-client-side-phishing-detection",
                    "--disable-component-extensions-with-background-pages",
                    "--disable-datasaver-prompt",
                    "--disable-default-apps",
                    "--disable-desktop-notifications",
                    "--disable-dev-shm-usage",
                    "--disable-domain-reliability",
                    "--disable-extensions",
                    "--disable-features=TranslateUI,BlinkGenPropertyTrees",
                    "--disable-hang-monitor",
                    "--disable-infobars",
                    "--disable-ipc-flooding-protection",
                    "--disable-notifications",
                    "--disable-popup-blocking",
                    "--disable-prompt-on-repost",
                    "--disable-renderer-backgrounding",
                    "--disable-setuid-sandbox",
                    "--disable-site-isolation-trials",
                    "--disable-sync",
                    "--disable-web-security",
                    "--enable-automation",
                    "--force-color-profile=srgb",
                    "--force-device-scale-factor=1",
                    "--ignore-certificate-errors",
                    "--js-flags=--random-seed=1157259157",
                    "--disable-logging",
                    "--metrics-recording-only",
                    "--mute-audio",
                    "--no-default-browser-check",
                    "--no-first-run",
                    "--no-sandbox",
                    "--password-store=basic",
                    "--test-type",
                    "--use-mock-keychain",
                ])
                // .env("DISPLAY", ":99")
                .kill_on_drop(true)
                .spawn()
                .unwrap()
                .wait_with_output()
                .await;

            debug!("ChromeDriver terminated with: {res:#?}");
        });

        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        let mut caps = DesiredCapabilities::chrome();
        caps.set_headless().unwrap();
        caps.set_no_sandbox().unwrap(); // Bypass OS security model
        caps.set_disable_dev_shm_usage().unwrap(); // overcome limited resource problems
        caps.set_disable_web_security().unwrap();
        caps.set_ignore_certificate_errors().unwrap();

        let webdriver = WebDriver::new(&format!("http://127.0.0.1:{port}"), caps)
            .await
            .unwrap();

        Ok(Self {
            _process,
            webdriver,
        })
    }
}

#[instrument]
async fn prepare() -> Result<ChromeWebDriver> {
    let webdriver = ChromeWebDriver::new().await.unwrap();

    let frontend_address = cli::manager::server_address();
    let webrtc_frontend_url = format!("http://{frontend_address}/webrtc");

    while let Err(error) = webdriver.goto(&webrtc_frontend_url).await {
        error!("Failed to connect: {error}");
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    // Wait for the system to stabilize
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    Ok(webdriver)
}

fn get_difference_map<K, V>(map1: &HashMap<K, V>, map2: &HashMap<K, V>) -> HashMap<K, V>
where
    K: std::hash::Hash + Eq + Copy,
    V: Clone,
{
    map1.iter()
        .filter(|(k, _)| !map2.contains_key(k))
        .map(|(k, v)| (*k, v.clone()))
        .collect()
}

#[instrument]
async fn task(session_cycles: i32) -> Result<()> {
    let webdriver = prepare().await?;

    let sessions_per_consumer = 5;

    let initial_tasks = helper::threads::process_tasks();
    info!(
        "Started webrtc test. Number of tasks: {}",
        initial_tasks.len()
    );

    for current_cycle in 0..=session_cycles {
        // Snapshot threads before creating any sessions this cycle.
        // Infrastructure threads from previous cycles are automatically
        // included, so only genuinely new (WebRTC-related) threads will
        // be tracked after teardown.
        let tasks_before_cycle = helper::threads::process_tasks();

        let add_consumer_button = webdriver.query(By::Id("add-consumer")).first().await?;
        add_consumer_button.click().await?;

        let add_session_button = webdriver.query(By::Id("add-session")).first().await?;
        for _ in 0..sessions_per_consumer {
            add_session_button.click().await?;
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        tokio::time::timeout(tokio::time::Duration::from_secs(30), {
            async {
                loop {
                    let elements = match webdriver
                        .webdriver
                        .query(By::Id("session-status"))
                        .with_text("Status: Playing")
                        .all_from_selector()
                        .await
                    {
                        Ok(elements) => elements,
                        Err(error) => break Err(error),
                    };

                    if elements.len().eq(&sessions_per_consumer) {
                        break Ok(());
                    }

                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        })
        .await??;

        info!("All sessions are Playing");

        let remove_consumer_button = webdriver.query(By::Id("remove-consumer")).first().await?;
        remove_consumer_button.click().await?;

        info!("Consumer removed, waiting for tasks to finish...");

        let surviving_threads = RwLock::new(HashMap::<u32, String>::new());

        let wait_for_tasks_to_die = async {
            loop {
                let current = helper::threads::process_tasks();
                let new_this_cycle = get_difference_map(&current, &tasks_before_cycle);
                *surviving_threads.write().await = new_this_cycle.clone();
                if new_this_cycle.is_empty() {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        };

        if tokio::time::timeout(tokio::time::Duration::from_secs(30), wait_for_tasks_to_die)
            .await
            .is_err()
        {
            if current_cycle > 0 {
                return Err(anyhow!(
                    "Thread leak detected on cycle {current_cycle}:\n{surviving_threads:#?}"
                ));
            }
        };

        let surviving_count = surviving_threads.read().await.len();
        let new_since_start =
            get_difference_map(&helper::threads::process_tasks(), &initial_tasks).len();

        info!("Successful cycles: {current_cycle}/{session_cycles}");
        info!("Current tasks: {}", helper::threads::process_tasks().len());
        info!("New tasks since start: {new_since_start}");
        info!("Surviving threads from this cycle: {surviving_count}");

        if surviving_count > 0 {
            info!("Surviving: {surviving_threads:#?}");
        }
    }

    Ok(())
}

#[instrument]
pub fn start_check_tasks_on_webrtc_reconnects() {
    std::thread::spawn(move || {
        let rt = Runtime::new().unwrap();

        info!("Starting WebRTC test...");
        if let Err(error) = rt.block_on(task(5)) {
            error!("WebRTC test failed: {error:?}");
            std::process::exit(1);
        }

        info!("WebRTC test passed!");
        std::process::exit(0);
    });
}
