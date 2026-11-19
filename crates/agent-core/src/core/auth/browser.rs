/// Open a URL in the default browser.
pub fn open_browser(url: &str) -> std::result::Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    {
        shell_open(url)?;
    }

    Ok(())
}

// ── Windows ──────────────────────────────────────────────────────────────────
//
// The URL goes to `ShellExecuteW` and nowhere else. It must never pass through
// a command interpreter: `cmd` ends a command at the first `&` (every OAuth
// authorize URL has several) and `start` reads a quoted first argument as a
// window title. `ShellExecuteW` takes the target as one string and hands it to
// the registered handler for the URL scheme, byte for byte.

/// What the Windows opener passes to `ShellExecuteW`: the verb and the target,
/// both NUL-terminated UTF-16. Built by [`shell_open_request`], which is plain
/// data so it is tested on every platform.
#[cfg(any(target_os = "windows", test))]
struct ShellOpen {
    verb: Vec<u16>,
    target: Vec<u16>,
}

/// Build the `ShellExecuteW` request for `url`, or say why it is refused.
///
/// `ShellExecuteW` with `open` runs whatever it is given: a program path, a
/// document, a `file:` URL. Only `http(s)` URLs are sign-in pages, so anything
/// else is refused instead of launched.
///
/// A `"`, whitespace (a space included) or control character (a NUL most of
/// all) is refused too. Sign-in URLs percent-encode all of them, and the
/// shell puts the URL into the browser's registered command line (`"%1"`, or
/// a bare `%1`): there a `"` closes the quote and a space starts a new
/// argument, so a crafted URL could pass the browser switches of its own.
/// The URL is not trimmed, quoted or escaped.
#[cfg(any(target_os = "windows", test))]
fn shell_open_request(url: &str) -> Result<ShellOpen, String> {
    let is_web_url = ["http://", "https://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    });
    if !is_web_url {
        return Err("Failed to open browser: only http(s) URLs are opened".to_string());
    }
    if url
        .chars()
        .any(|c| c == '"' || c.is_whitespace() || c.is_control())
    {
        return Err(
            "Failed to open browser: URL contains a quote, whitespace or control character"
                .to_string(),
        );
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    Ok(ShellOpen {
        verb: wide("open"),
        target: wide(url),
    })
}

/// The error for a `ShellExecuteW` return value of 32 or less.
#[cfg(any(target_os = "windows", test))]
fn shell_execute_failure(code: isize) -> String {
    // SE_ERR_NOASSOC / SE_ERR_ASSOCINCOMPLETE
    let hint = match code {
        27 | 31 => ": no default browser is set for http links",
        _ => "",
    };
    format!("Failed to open browser: ShellExecuteW returned {code}{hint}")
}

/// Open `url` through the shell, on a short-lived thread of its own.
///
/// `ShellExecuteW` may call shell extensions through COM, so the caller is
/// asked to initialise COM first. Doing that here, on a thread that ends right
/// after, leaves the calling thread (often a Tokio worker) in no apartment it
/// never asked for.
///
/// This blocks until the shell returns, so the caller learns whether it
/// worked. That is normally a few milliseconds (the shell starts the browser
/// and does not wait for it), but a handler that shows a dialog can hold it
/// longer. That is acceptable because every sign-in prints its URL *before*
/// calling [`open_browser`], so the manual fallback is on screen whatever the
/// handler does.
#[cfg(target_os = "windows")]
fn shell_open(url: &str) -> Result<(), String> {
    let request = shell_open_request(url)?;
    std::thread::Builder::new()
        .name("open-browser".into())
        .spawn(move || shell_execute(&request))
        .map_err(|e| format!("Failed to open browser: {e}"))?
        .join()
        .map_err(|_| "Failed to open browser: the launcher thread panicked".to_string())?
}

#[cfg(target_os = "windows")]
fn shell_execute(request: &ShellOpen) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // SAFETY: a null reserved pointer and valid COINIT flags. Success (S_OK or
    // S_FALSE, both >= 0) is balanced by the `CoUninitialize` below; a failure
    // is not, and the shell call is still made.
    let com_ready = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    } >= 0;

    // SAFETY: `verb` and `target` are NUL-terminated UTF-16 that live past the
    // call; the owner window, parameters and directory are null (none).
    let instance = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            request.verb.as_ptr(),
            request.target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    if com_ready {
        // SAFETY: balances the successful `CoInitializeEx` on this thread.
        unsafe { CoUninitialize() };
    }

    // The documented result: an HINSTANCE above 32 is success, anything else an
    // error code.
    let code = instance as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(shell_execute_failure(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target as the shell receives it: the UTF-16 up to the terminating NUL.
    fn decode(wide: &[u16]) -> String {
        let (nul, text) = wide.split_last().expect("NUL-terminated");
        assert_eq!(*nul, 0, "must end in a NUL");
        assert!(!text.contains(&0), "must hold no other NUL");
        String::from_utf16(text).expect("valid UTF-16")
    }

    /// The shape of the URLs the sign-ins open: several `&`, percent escapes,
    /// a `%20`, and a `redirect_uri` that is itself a URL.
    const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize?code=true\
        &client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code\
        &redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback\
        &scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference\
        &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
        &code_challenge_method=S256&state=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

    #[test]
    fn url_reaches_shell_execute_byte_for_byte() {
        let request = shell_open_request(AUTHORIZE_URL).expect("an https URL is accepted");
        assert_eq!(
            decode(&request.target),
            AUTHORIZE_URL,
            "`&` and everything after it survive"
        );
        assert_eq!(decode(&request.verb), "open");
    }

    #[test]
    fn url_is_not_quoted_or_given_a_window_title() {
        // `start "<url>"` would make the URL the window title; any quoting added
        // here would reach the browser as part of the URL.
        let request = shell_open_request(AUTHORIZE_URL).unwrap();
        let target = decode(&request.target);
        assert!(!target.contains('"'));
        assert!(target.starts_with("https://claude.ai/"));
    }

    #[test]
    fn scheme_is_matched_case_insensitively() {
        for url in [
            "HTTPS://example.com/a?b=1&c=2",
            "Http://localhost:1455/auth?x=1&y=2",
        ] {
            let request = shell_open_request(url).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert_eq!(decode(&request.target), url);
        }
    }

    #[test]
    fn non_ascii_url_is_encoded_as_utf16() {
        // 😀 is outside the BMP, so it must arrive as a surrogate pair: a byte
        // copy of the UTF-8 would not.
        let url = "https://example.com/?q=é😀&r=1";
        let request = shell_open_request(url).unwrap();
        assert!(request.target.iter().any(|u| (0xD800..0xE000).contains(u)));
        assert_eq!(decode(&request.target), url);
    }

    #[test]
    fn only_web_urls_are_opened() {
        // `ShellExecuteW("open", ..)` would run a program or a document for any
        // of these.
        for bad in [
            "",
            "calc.exe",
            r"C:\Windows\System32\calc.exe",
            r"\\host\share\payload.exe",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:network",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "httpx://example.com",
            "http:/example.com",
            " https://example.com",
            "xhttps://example.com",
            "ht",
            "héllo://x",
        ] {
            assert!(shell_open_request(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn control_characters_are_refused() {
        for bad in [
            "https://example.com/\0calc.exe",
            "https://example.com/\r\nHost: evil",
            "https://example.com/a\tb",
            "https://example.com/\u{7f}",
            "https://example.com/\u{85}",
        ] {
            assert!(shell_open_request(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn quotes_and_whitespace_are_refused() {
        // A registered `"%1"` would see the `"` close its quote, and the space
        // (or any other whitespace) start a new argument.
        for bad in [
            "https://example.com/a b",
            "https://example.com/ ",
            "https://example.com/?q=1 --new-window",
            "https://example.com/\"",
            "https://example.com/\" --user-data-dir=C:\\evil",
            "https://example.com/a\u{a0}b",
            "https://example.com/a\u{2028}b",
            "https://example.com/a\u{3000}b",
        ] {
            assert!(shell_open_request(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn encoded_quotes_and_spaces_are_fine() {
        // What the sign-in URLs actually carry: the same characters, encoded.
        let url = "https://example.com/?q=a%20b%22c+d&r=e";
        let request = shell_open_request(url).unwrap();
        assert_eq!(decode(&request.target), url);
    }

    #[test]
    fn shell_execute_failure_names_the_code() {
        assert!(shell_execute_failure(2).contains("returned 2"));
        assert!(shell_execute_failure(31).contains("no default browser"));
        assert!(shell_execute_failure(31).starts_with("Failed to open browser"));
    }

    /// The bug this file exists to prevent: opening a URL through `cmd /c start`.
    /// Whatever the Windows path becomes, it must not go back through a command
    /// interpreter (or a PowerShell/Explorer detour that parses the URL again).
    ///
    /// Only the code above the test module is read, and only its code: comment
    /// lines are dropped, so a comment can neither trip the ban nor stand in
    /// for the `ShellExecuteW` call.
    #[test]
    fn windows_path_does_not_go_through_a_command_interpreter() {
        let source = include_str!("browser.rs");
        let (production, _tests) = source
            .split_once("\n#[cfg(test)]\nmod tests")
            .expect("the tests module follows the production code");
        let code = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        for banned in ["\"cmd", "\"/c\"", "\"start\"", "powershell", "explorer"] {
            assert!(
                !code.contains(banned),
                "browser.rs must not use {banned}: it would split the URL at the first `&`"
            );
        }
        assert!(
            code.contains("shellexecutew("),
            "the Windows path must call ShellExecuteW"
        );
    }
}
