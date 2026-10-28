//! The wall clock, formatted the way log lines want it.

#![allow(unsafe_code)]

/// Local time as `HH:MM:SS`, or `--:--:--` if the clock is unavailable.
pub fn local_hms() -> String {
    let mut now: libc::time_t = 0;
    // SAFETY: `now` is a valid `time_t`.
    unsafe { libc::time(&mut now) };

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    let ok = unsafe { libc::localtime_r(&now, &mut tm) };
    if ok.is_null() {
        return "--:--:--".to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    #[test]
    fn looks_like_a_time() {
        let stamp = super::local_hms();
        assert_eq!(stamp.len(), 8, "{stamp}");
        assert_eq!(stamp.matches(':').count(), 2, "{stamp}");
    }
}
