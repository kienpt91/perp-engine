//! Process resource probe: resident memory from `/proc/self/status`.
//!
//! `VmRSS` is reported in kB by the kernel, so no page-size assumption is
//! needed. On systems without that file the probe reports nothing.

use perp_app::simulation::ResourceProbe;

/// Reads `VmRSS` of the current process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcStatusProbe;

/// Extract resident bytes from the text of `/proc/<pid>/status`.
pub fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let mut parts = line.split_whitespace().skip(1);
    let kb: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") => kb.checked_mul(1024),
        _ => None,
    }
}

impl ResourceProbe for ProcStatusProbe {
    fn resident_bytes(&self) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        parse_vm_rss(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_kernel_format() {
        let text = "Name:\tx\nVmPeak:\t  200 kB\nVmRSS:\t    1234 kB\nThreads:\t1\n";
        assert_eq!(parse_vm_rss(text), Some(1234 * 1024));
    }

    #[test]
    fn rejects_malformed_or_missing_lines() {
        assert_eq!(parse_vm_rss(""), None);
        assert_eq!(parse_vm_rss("VmRSS:\tabc kB\n"), None);
        assert_eq!(parse_vm_rss("VmRSS:\t12 MB\n"), None);
        assert_eq!(parse_vm_rss("VmRSS:\n"), None);
        assert_eq!(parse_vm_rss(&format!("VmRSS: {} kB\n", u64::MAX)), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_this_process_on_linux() {
        let rss = ProcStatusProbe.resident_bytes().unwrap();
        assert!(rss > 100 * 1024, "{rss}");
    }
}
