// Portions of this file are derived from code that is
// Copyright © 2015 Sebastian Thiel
// <https://github.com/Byron/open-rs>

#[cfg(not(windows))]
fn open_url_candidates(url: &str) -> Vec<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        vec![vec!["/usr/bin/open".to_string(), url.to_string()]]
    }

    #[cfg(not(target_os = "macos"))]
    {
        vec![
            vec!["xdg-open".to_string(), url.to_string()],
            vec!["gio".to_string(), "open".to_string(), url.to_string()],
            vec!["gnome-open".to_string(), url.to_string()],
            vec!["kde-open".to_string(), url.to_string()],
            vec!["wslview".to_string(), url.to_string()],
        ]
    }
}

#[cfg(not(windows))]
fn open_with_args(url: &str, app: &str) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "/usr/bin/open".to_string(),
            "-a".to_string(),
            app.to_string(),
            url.to_string(),
        ]
    }

    #[cfg(not(target_os = "macos"))]
    {
        vec![app.to_string(), url.to_string()]
    }
}

#[cfg(not(windows))]
pub fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        for candidate in open_url_candidates(&url) {
            let mut cmd = std::process::Command::new(&candidate[0]);
            cmd.args(&candidate[1..]);

            if let Ok(status) = cmd.status() {
                if status.success() {
                    return;
                }
            }
        }
    });
}

#[cfg(not(windows))]
pub fn open_with(url: &str, app: &str) {
    let url = url.to_string();
    let app = app.to_string();

    std::thread::spawn(move || {
        let args = open_with_args(&url, &app);

        let mut cmd = std::process::Command::new(args[0]);
        cmd.args(&args[1..]);

        let _ = cmd.status();
    });
}

#[cfg(windows)]
fn shell_execute(url: String, with: Option<String>) {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::shellapi::ShellExecuteW;
    /// Convert a rust string to a windows wide string
    fn wide_string(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    std::thread::spawn(move || {
        let operation = wide_string("open");

        let url = wide_string(&url);
        let with = with.map(|s| wide_string(&s));

        let (app, path) = match with {
            Some(app) => (app.as_ptr(), url.as_ptr()),
            None => (url.as_ptr(), std::ptr::null()),
        };

        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                app,
                path,
                std::ptr::null(),
                winapi::um::winuser::SW_SHOW,
            )
        };
        // ShellExecuteW returns an HINSTANCE > 32 on success
        if (result as usize) <= 32 {
            log::error!("ShellExecuteW failed with code {}", result as usize);
        }
    });
}

#[cfg(windows)]
pub fn open_url(url: &str) {
    shell_execute(url.to_string(), None);
}

#[cfg(windows)]
pub fn open_with(url: &str, app: &str) {
    shell_execute(url.to_string(), Some(app.to_string()));
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_small_string() -> impl Strategy<Value = String> {
        proptest::collection::vec(any::<char>(), 0..24)
            .prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn open_url_candidates_preserve_requested_url(url in arb_small_string()) {
            let candidates = open_url_candidates(&url);
            prop_assert!(!candidates.is_empty());
            for candidate in candidates {
                prop_assert_eq!(candidate.last().map(String::as_str), Some(url.as_str()));
            }
        }

        #[test]
        fn open_with_args_preserve_app_and_url(
            url in arb_small_string(),
            app in arb_small_string(),
        ) {
            let args = open_with_args(&url, &app);

            #[cfg(target_os = "macos")]
            {
                prop_assert_eq!(args, vec![
                    "/usr/bin/open".to_string(),
                    "-a".to_string(),
                    app,
                    url,
                ]);
            }

            #[cfg(not(target_os = "macos"))]
            {
                prop_assert_eq!(args, vec![app, url]);
            }
        }
    }
}
