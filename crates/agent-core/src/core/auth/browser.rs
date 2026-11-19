//! Opening a URL in the default browser. The URL reaches the OS as one whole value, never through
//! a shell that parses it. Windows used `cmd /c start <url>`: cmd ends a command at `&`, so every
//! OAuth sign-in tab opened at the authorize URL's first `&`, without `client_id`, `state` or the
//! PKCE challenge, for all seven providers. (Quoting the URL isn't enough there: `start` takes a
//! first quoted argument as the window title, and cmd expands `%…%` even inside quotes, which a
//! percent-encoded URL is full of.) Windows now hands the URL to `ShellExecuteW`, the shell's own
//! "open" (what `Start-Process` and `os.startfile` call): no command line at all.

use std::ffi::OsString;

/// How a URL is handed to the OS. The URL is always one whole value.
#[derive(Debug, PartialEq, Eq)]
enum Opener {
    /// A program with the URL as its only argument (no shell in between).
    Program {
        program: &'static str,
        url: OsString,
    },
    /// Windows: `ShellExecuteW(NULL, "open", url, …)`, the URL as NUL-terminated UTF-16.
    ShellExecute { url: Vec<u16> },
}

#[derive(Clone, Copy, Debug)]
enum Platform {
    Linux,
    MacOs,
    Windows,
    Other,
}

fn this_platform() -> Platform {
    if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Other
    }
}

/// The opener for `url` on `os` (None: no opener there; the caller prints the URL anyway).
fn opener_for(os: Platform, url: &str) -> std::result::Result<Option<Opener>, String> {
    Ok(match os {
        Platform::Linux => Some(Opener::Program {
            program: "xdg-open",
            url: url.into(),
        }),
        Platform::MacOs => Some(Opener::Program {
            program: "open",
            url: url.into(),
        }),
        Platform::Windows => {
            if url.contains('\0') {
                return Err("Failed to open browser: the URL contains a NUL".to_string());
            }
            Some(Opener::ShellExecute {
                url: url.encode_utf16().chain(Some(0)).collect(),
            })
        }
        Platform::Other => None,
    })
}

/// Open a URL in the default browser.
pub fn open_browser(url: &str) -> std::result::Result<(), String> {
    match opener_for(this_platform(), url)? {
        Some(opener) => run(opener),
        None => Ok(()),
    }
}

fn run(opener: Opener) -> std::result::Result<(), String> {
    match opener {
        Opener::Program { program, url } => std::process::Command::new(program)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|e| format!("Failed to open browser: {}", e)),
        Opener::ShellExecute { url } => shell_execute(url),
    }
}

/// `ShellExecuteW` on a thread of its own: the shell may hand the URL to a COM handler, so COM is
/// set up first (apartment-threaded, as its documentation asks), and the caller's thread (an async
/// runtime's worker) isn't this function's to set up.
#[cfg(windows)]
fn shell_execute(url: Vec<u16>) -> std::result::Result<(), String> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    std::thread::spawn(move || {
        let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
        // SAFETY: plain Win32 calls; `verb` and `url` are NUL-terminated UTF-16 that outlive them.
        let code = unsafe {
            let com = CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            );
            let h = ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                url.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
            if com >= 0 {
                CoUninitialize();
            }
            h as isize
        };
        // Above 32 is success (ShellExecute's documented convention); below, an error code.
        if code > 32 {
            Ok(())
        } else {
            Err(format!(
                "Failed to open browser: ShellExecute error {}",
                code
            ))
        }
    })
    .join()
    .map_err(|_| "Failed to open browser: the opener thread panicked".to_string())?
}

#[cfg(not(windows))]
fn shell_execute(_url: Vec<u16>) -> std::result::Result<(), String> {
    Err("Failed to open browser: ShellExecute is Windows-only".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every provider's sign-in URL as its own code builds it: the four authorize URLs (each with
    /// several `&`), and the three device flows' verification URLs.
    fn provider_urls() -> Vec<(&'static str, String)> {
        use super::super::google_gemini::{build_authorize_url, GeminiRegistration};
        let gemini =
            GeminiRegistration::test("synaps-desktop.example", Some("public-client")).unwrap();
        vec![
            (
                "anthropic",
                super::super::build_auth_url("challenge-xyz", "state-123", 53692),
            ),
            (
                "openai-codex",
                super::super::openai_codex::build_auth_url("challenge-xyz", "state-123").unwrap(),
            ),
            (
                "xai-auth",
                super::super::xai::authorize_url(
                    "https://auth.x.ai/authorize",
                    "challenge-xyz",
                    "state-123",
                    "nonce-456",
                )
                .unwrap(),
            ),
            (
                "google-gemini",
                build_authorize_url(&gemini, "challenge-xyz", "state-123", 45289),
            ),
            (
                "github-copilot",
                "https://github.com/login/device?user_code=ABCD-1234".to_string(),
            ),
            (
                "kimi-code",
                "https://www.kimi.com/code/device?user_code=ABCD-EFGH".to_string(),
            ),
            (
                "muse",
                "https://auth.meta.com/device?user_code=ABCD-EFGH".to_string(),
            ),
        ]
    }

    /// What the OS gets, as text: a program's one argument, or ShellExecute's file (one NUL, at
    /// the end).
    fn handed_over(opener: &Opener) -> String {
        match opener {
            Opener::Program { url, .. } => url.to_str().unwrap().to_owned(),
            Opener::ShellExecute { url } => {
                let (last, text) = url.split_last().unwrap();
                assert_eq!(*last, 0, "NUL-terminated");
                assert!(!text.contains(&0), "no NUL inside: it would cut the URL");
                String::from_utf16(text).unwrap()
            }
        }
    }

    #[test]
    fn the_whole_url_reaches_the_opener_for_every_provider_on_every_os() {
        let urls = provider_urls();
        let with_amp = urls
            .iter()
            .filter(|(_, u)| u.matches('&').count() >= 3)
            .count();
        assert!(
            with_amp >= 4,
            "the four authorize URLs carry several `&` (what Windows cut at)"
        );
        for (provider, url) in &urls {
            for os in [Platform::Linux, Platform::MacOs, Platform::Windows] {
                let opener = opener_for(os, url).unwrap().expect("an opener");
                assert_eq!(
                    &handed_over(&opener),
                    url,
                    "{provider} on {os:?}: the whole URL"
                );
            }
            // Windows: no command line at all (cmd split the URL at `&` and expands `%…%`).
            assert!(
                matches!(
                    opener_for(Platform::Windows, url),
                    Ok(Some(Opener::ShellExecute { .. }))
                ),
                "{provider}"
            );
        }
    }

    #[test]
    fn a_program_gets_the_url_as_its_only_argument() {
        let url = "https://claude.ai/oauth/authorize?code=true&client_id=x&state=s%26t";
        assert_eq!(
            opener_for(Platform::Linux, url),
            Ok(Some(Opener::Program {
                program: "xdg-open",
                url: url.into()
            }))
        );
        assert_eq!(
            opener_for(Platform::MacOs, url),
            Ok(Some(Opener::Program {
                program: "open",
                url: url.into()
            }))
        );
        assert_eq!(opener_for(Platform::Other, url), Ok(None));
        assert!(
            opener_for(Platform::Windows, "https://a.example/\0b").is_err(),
            "a NUL would cut it"
        );
    }
}
