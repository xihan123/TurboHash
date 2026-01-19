pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let seconds = ms / 1000;
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}
