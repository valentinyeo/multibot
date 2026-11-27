use crate::{app::MultibotView, state::SharedAppStore};
use gpui_kit::{App, Context};
use multibot_core::account::{AccountClient, AccountSession, DEFAULT_ACCOUNT_ORIGIN};

pub(crate) struct AccountUi {
    pub email: Option<String>,
    pub status: String,
    pub busy: bool,
    pub startup_started: bool,
    pub deepgram_key: Option<String>,
}

impl std::fmt::Debug for AccountUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountUi")
            .field("email", &self.email)
            .field("status", &self.status)
            .field("busy", &self.busy)
            .field("deepgram_key", &"[redacted]")
            .finish()
    }
}

impl Default for AccountUi {
    fn default() -> Self {
        Self {
            email: None,
            status: "Signed out. Sign in to sync your Deepgram key.".into(),
            busy: false,
            startup_started: false,
            deepgram_key: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AccountJob {
    SignIn,
    Sync,
    UploadKey,
    SignOut,
}

impl MultibotView {
    pub(crate) fn start_account_job(&mut self, job: AccountJob, cx: &mut Context<Self>) {
        let store = cx.global_mut::<SharedAppStore>();
        if store.account.busy {
            return;
        }
        if matches!(job, AccountJob::SignIn) && store.account.email.is_some() {
            self.notice = Some("You are already signed in. Sign out to switch accounts.".into());
            cx.notify();
            return;
        }
        if !store.persist_to_disk {
            self.notice = Some("Account sign-in requires Multibot's saved profile.".into());
            cx.notify();
            return;
        }
        store.account.busy = true;
        store.account.status = match job {
            AccountJob::SignIn => "Complete sign-in in your browser (up to three minutes)…",
            AccountJob::SignOut => "Signing out…",
            AccountJob::UploadKey => "Saving Deepgram key to account…",
            AccountJob::Sync => "Syncing Deepgram key…",
        }
        .into();
        store.bump_revision();
        let voice = self.settings.voice.clone();
        let worker = self.runtime.spawn_blocking(move || {
            let (voice, _) = voice.resolved(None);
            run_job(job, &voice.api_key)
        });
        cx.spawn(async move |_, cx| {
            let result = worker.await;
            let _ = cx.update(|cx: &mut App| {
                let store = cx.global_mut::<SharedAppStore>();
                store.account.busy = false;
                match result {
                    Ok(Ok((email, key, status))) => {
                        store.account.email = email;
                        store.account.deepgram_key = key;
                        store.account.status = status;
                    }
                    Ok(Err((email, error))) => {
                        if email.is_some() {
                            store.account.email = email;
                        }
                        store.account.status = error;
                    }
                    Err(_) => store.account.status = "Account task stopped. Try again.".into(),
                }
                store.bump_revision();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn start_account_once(&mut self, cx: &mut Context<Self>) {
        let store = cx.global_mut::<SharedAppStore>();
        if !store.persist_to_disk || store.account.startup_started {
            return;
        }
        store.account.startup_started = true;
        if AccountSession::path().exists() {
            self.start_account_job(AccountJob::Sync, cx);
        }
    }
}

type JobResult = Result<(Option<String>, Option<String>, String), (Option<String>, String)>;

fn run_job(job: AccountJob, local_key: &str) -> JobResult {
    let path = AccountSession::path();
    let client = if matches!(job, AccountJob::SignIn | AccountJob::SignOut) {
        AccountClient::signed_out(DEFAULT_ACCOUNT_ORIGIN, path)
    } else {
        AccountClient::load()
    };
    let mut client = client.map_err(|e| (None, e.to_string()))?;
    if matches!(job, AccountJob::SignOut) {
        client.sign_out().map_err(|e| (None, e.to_string()))?;
        return Ok((
            None,
            None,
            "Signed out. Saved account tokens cleared.".into(),
        ));
    }
    let result = (|| -> anyhow::Result<(Option<String>, String)> {
        if matches!(job, AccountJob::SignIn) {
            client.sign_in(open_browser)?;
        }
        if client.session.is_none() {
            anyhow::bail!("Sign in to your Multibot account first.");
        }
        if matches!(job, AccountJob::UploadKey) {
            client.save_deepgram_key(local_key)?;
            return Ok((
                Some(local_key.to_owned()),
                "Deepgram key saved to account".into(),
            ));
        }
        match client.fetch_deepgram_key()? {
            Some(key) => Ok((Some(key), "Deepgram key synced from account".into())),
            None if !local_key.is_empty() => {
                client.save_deepgram_key(local_key)?;
                Ok((
                    Some(local_key.to_owned()),
                    "Deepgram key saved to account".into(),
                ))
            }
            None => Ok((
                None,
                "No Deepgram key in account. Add one in Voice settings.".into(),
            )),
        }
    })();
    let email = client.session.as_ref().map(|s| {
        if s.email.is_empty() {
            "Multibot account".into()
        } else {
            s.email.clone()
        }
    });
    match result {
        Ok((key, status)) => Ok((email, key, status)),
        Err(error) => Err((email, error.to_string())),
    }
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> anyhow::Result<()> {
    use std::ffi::c_void;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            window: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let url: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as usize <= 32 {
        anyhow::bail!("Could not open your browser. Try sign-in again.");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_browser(url: &str) -> anyhow::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| anyhow::anyhow!("Could not open your browser. Try sign-in again."))?;
    if !status.success() {
        anyhow::bail!("Could not open your browser. Try sign-in again.");
    }
    Ok(())
}
