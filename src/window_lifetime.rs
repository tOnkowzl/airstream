//! Native window liveness is independent of Chromium renderer timers.
//! A minimized app window is still visible to IsWindowVisible and remains alive.
#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    type Hwnd = *mut c_void;
    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: unsafe extern "system" fn(Hwnd, isize) -> i32, param: isize) -> i32;
        fn GetWindowTextW(window: Hwnd, text: *mut u16, capacity: i32) -> i32;
        fn GetClassNameW(window: Hwnd, text: *mut u16, capacity: i32) -> i32;
        fn IsWindowVisible(window: Hwnd) -> i32;
    }

    unsafe extern "system" fn inspect(window: Hwnd, param: isize) -> i32 {
        if IsWindowVisible(window) == 0 { return 1 }
        let mut title = [0u16; 512];
        let len = GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32);
        if len <= 0 { return 1 }
        if String::from_utf16_lossy(&title[..len as usize]) != "Airstream" { return 1 }
        let mut class = [0u16; 128];
        let len = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
        if len > 0 && String::from_utf16_lossy(&class[..len as usize]) == "Chrome_WidgetWin_1" {
            *(param as *mut bool) = true;
            return 0;
        }
        1
    }

    pub fn exists() -> bool {
        let mut found = false;
        // SAFETY: EnumWindows invokes the callback synchronously; the bool outlives it.
        unsafe { EnumWindows(inspect, &mut found as *mut bool as isize); }
        found
    }
}

pub async fn wait_until_closed() {
    let started = std::time::Instant::now();
    let mut seen_window = false;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        #[cfg(windows)]
        let present = windows::exists();
        #[cfg(not(windows))]
        let present = true;
        if present {
            seen_window = true;
        } else if seen_window || started.elapsed() > std::time::Duration::from_secs(60) {
            return;
        }
    }
}
