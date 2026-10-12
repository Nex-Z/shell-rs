//! Read-only Linux GPU sampling. The UI owns when to sample and stale readings.
//!
//! NVIDIA uses one bounded CSV query for all devices. DRM/PCI sysfs supplies
//! discovery and the metrics the kernel makes readable without extra privilege.
//! Missing metrics are unknown, never zero. No driver or host setting is changed.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Problem {
    MissingTool,
    PermissionDenied,
    ToolFailed,
    MetricsUnavailable,
}

/// One physical PCI device. Memory is bytes, temperature Celsius and power watts.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub(super) struct Device {
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// Aggregate GPU activity in percent, not memory-controller activity.
    pub utilization: Option<f32>,
    pub memory_used: Option<u64>,
    pub memory_total: Option<u64>,
    pub temperature: Option<f32>,
    pub power: Option<f32>,
    pub problem: Option<Problem>,
}

// Delimiters and statuses are on stdout because SSH Exec currently discards the
// remote exit code and stderr. Bound both query time and output. Without timeout
// or head we retain NVIDIA discovery but do not start an unbounded vendor tool.
// The single-quoted sh payload contains neither single quotes nor ! (csh).
const SCRIPT: &str = r#"
export LC_ALL=C;
printf "@@shellrs-gpu-v1\n@@nvidia-begin\n";
if command -v nvidia-smi >/dev/null 2>&1; then
    if command -v timeout >/dev/null 2>&1 && command -v head >/dev/null 2>&1; then
        (timeout -s KILL 2 nvidia-smi --query-gpu=pci.bus_id,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw --format=csv,noheader,nounits 2>/dev/null; printf "\n@@nvidia-status=%s\n" "$?") | head -c 16384;
    else printf "@@nvidia-status=unavailable\n"; fi;
else printf "@@nvidia-status=missing\n"; fi;
printf "\n@@sysfs-begin\n";
read_value() {
    value=; read_state=ok;
    if test -e "$1"; then
        if test -r "$1"; then
            IFS= read -r value < "$1" 2>/dev/null || read_state=failed;
            if test "${#value}" -gt 160; then value=; read_state=failed; fi;
        else read_state=denied; fi;
    fi;
};
metric() {
    read_value "$1";
    if test "${#value}" -gt 24; then value=; read_state=failed; fi;
    case "$read_state" in denied) problem=denied;; failed) if test "$problem" = ok; then problem=failed; fi;; esac;
};
sysfs_state=ok;
if test -r /sys/bus/pci/devices && test -x /sys/bus/pci/devices; then :;
elif test -r /sys/class/drm && test -x /sys/class/drm; then :;
else sysfs_state=unavailable; fi;
count=0; devices=0;
for directory in /sys/bus/pci/devices/* /sys/class/drm/card[0-9]*/device; do
    test -d "$directory" || continue;
    if test -r "$directory" && test -x "$directory"; then :;
    else sysfs_state=denied; continue; fi;
    count=$((count + 1));
    if test "$count" -gt 4096; then sysfs_state=limit; break; fi;
    read_value "$directory/class";
    case "$read_state" in denied|failed) sysfs_state=$read_state;; esac;
    case "$value" in 0x03*) ;; *) continue;; esac;
    read_value "$directory/vendor";
    case "$read_state" in denied|failed) sysfs_state=$read_state;; esac;
    case "$value" in 0x10de|0x10DE) vendor=NVIDIA;; 0x1002) vendor=AMD;; 0x8086) vendor=Intel;; *) continue;; esac;
    devices=$((devices + 1));
    if test "$devices" -gt 128; then sysfs_state=limit; break; fi;
    id=${directory##*/};
    if test "$id" = device; then
        id=; lines=0;
        if test -r "$directory/uevent"; then
            while IFS= read -r line; do
                lines=$((lines + 1));
                case "$line" in PCI_SLOT_NAME=*) id=${line#PCI_SLOT_NAME=}; break;; esac;
                if test "$lines" -ge 64; then break; fi;
            done < "$directory/uevent";
        fi;
    fi;
    if test -z "$id" || test "${#id}" -gt 16; then sysfs_state=failed; continue; fi;
    problem=ok;
    read_value "$directory/device"; product=$value;
    if test "${#product}" -gt 6; then product=; fi;
    read_value "$directory/product_name"; name=$value;
    busy=; used=; total=; temp=; power=; discrete=no;
    if test "$vendor" = AMD; then
        read_value "$directory/board_info";
        case "$value" in "type : cem"|"type : oam") discrete=yes;; esac;
    fi;
    if test "$vendor" = AMD; then
        metric "$directory/gpu_busy_percent"; busy=$value;
        metric "$directory/mem_info_vram_used"; used=$value;
        metric "$directory/mem_info_vram_total"; total=$value;
    fi;
    monitors=0;
    for hwmon in "$directory"/hwmon/hwmon*; do
        test -d "$hwmon" || continue;
        monitors=$((monitors + 1)); if test "$monitors" -gt 16; then break; fi;
        read_value "$hwmon/name";
        case "$vendor:$value" in AMD:amdgpu|AMD:radeon|Intel:i915|Intel:xe|NVIDIA:nouveau) ;; *) continue;; esac;
        sensor=$value;
        if test "$vendor:$sensor" = Intel:xe; then
            metric "$hwmon/temp2_input"; temp=$value;
        else metric "$hwmon/temp1_input"; temp=$value; fi;
        if test "$discrete" = yes; then
            metric "$hwmon/power1_average"; power=$value;
            if test -z "$power"; then metric "$hwmon/power1_input"; power=$value; fi;
        fi;
        break;
    done;
    printf "@@device\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$id" "$vendor" "$product" "$busy" "$used" "$total" "$temp" "$power" "$problem" "$name";
done;
printf "@@sysfs-status=%s\n@@shellrs-gpu-end\n" "$sysfs_state";
"#;

/// A fixed read-only command; remote data is never substituted back into shell.
pub(super) fn command() -> String {
    format!(
        "sh -c '{}'",
        SCRIPT.lines().map(str::trim).collect::<Vec<_>>().join(" ")
    )
}

/// Parse a complete sample, rejecting truncation rather than reporting no GPUs.
pub(super) fn parse(output: &str) -> Result<Vec<Device>, String> {
    if output.len() > 131_072 {
        return Err("GPU sample exceeds its output limit".into());
    }
    let mut lines = output.lines().map(|line| line.trim_end_matches('\r'));
    if lines.next() != Some("@@shellrs-gpu-v1") || lines.next() != Some("@@nvidia-begin") {
        return Err("GPU sample did not start".into());
    }
    let mut nvidia_rows = Vec::new();
    let mut nvidia_status = None;
    let mut sysfs_rows = Vec::new();
    let mut sysfs_status = None;
    let mut in_sysfs = false;
    let mut ended = false;
    for line in lines {
        if ended {
            if !line.trim().is_empty() {
                return Err("Unexpected data after GPU sample".into());
            }
        } else if line == "@@shellrs-gpu-end" {
            ended = true;
        } else if line == "@@sysfs-begin" && !in_sysfs && nvidia_status.is_some() {
            in_sysfs = true;
        } else if let Some(status) = line.strip_prefix("@@nvidia-status=") {
            if in_sysfs || nvidia_status.replace(status).is_some() {
                return Err("Invalid NVIDIA sample status".into());
            }
        } else if let Some(status) = line.strip_prefix("@@sysfs-status=") {
            if !in_sysfs || sysfs_status.replace(status).is_some() {
                return Err("Invalid GPU discovery status".into());
            }
        } else if !line.trim().is_empty() {
            if in_sysfs && sysfs_status.is_none() && line.starts_with("@@device\t") {
                sysfs_rows.push(line);
            } else if !in_sysfs && nvidia_status.is_none() {
                nvidia_rows.push(line);
            } else {
                return Err("Invalid GPU sample framing".into());
            }
        }
    }
    if !ended || !in_sysfs {
        return Err("GPU sample was interrupted".into());
    }
    let Some(nvidia_status) = nvidia_status else {
        return Err("NVIDIA sample status is missing".into());
    };
    match sysfs_status {
        Some("ok" | "unavailable") => {}
        Some("denied") => return Err("GPU discovery permission denied".into()),
        _ => return Err("GPU discovery failed or exceeded its limit".into()),
    }
    let nvidia_problem = match nvidia_status {
        "0" => None,
        "missing" => Some(Problem::MissingTool),
        "4" => Some(Problem::PermissionDenied),
        "unavailable" => Some(Problem::ToolFailed),
        status if status.parse::<u8>().is_ok() => Some(Problem::ToolFailed),
        _ => return Err("Invalid NVIDIA sample status".into()),
    };
    let mut devices = BTreeMap::<String, Device>::new();
    for row in sysfs_rows {
        let device = parse_sysfs(row, nvidia_problem)?;
        // PCI and DRM paths often refer to the same device. Prefer a row with
        // telemetry over discovery only, without adding a second physical GPU.
        if let Some(existing) = devices.get_mut(&device.id) {
            if existing.vendor != device.vendor {
                return Err("Conflicting GPU vendor for PCI address".into());
            }
            merge_metrics(existing, device);
        } else {
            devices.insert(device.id.clone(), device);
        }
    }
    if nvidia_status == "0" {
        for row in nvidia_rows {
            let mut device = parse_nvidia(row)?;
            if let Some(fallback) = devices.remove(&device.id) {
                if fallback.vendor != "NVIDIA" {
                    return Err("Conflicting GPU vendor for PCI address".into());
                }
                merge_metrics(&mut device, fallback);
            }
            devices.insert(device.id.clone(), device);
        }
    }
    if devices.len() > 64 {
        return Err("GPU sample exceeds its device limit".into());
    }
    if devices.is_empty() {
        if sysfs_status != Some("ok") {
            return Err("GPU discovery is unavailable".into());
        }
        if !matches!(nvidia_status, "0" | "missing") {
            return Err("NVIDIA GPU query failed".into());
        }
    }
    for device in devices.values_mut() {
        validate_memory(device);
        if device.problem.is_none() && !has_metrics(device) {
            device.problem = Some(Problem::MetricsUnavailable);
        }
    }
    Ok(devices.into_values().collect())
}

fn parse_sysfs(row: &str, nvidia_problem: Option<Problem>) -> Result<Device, String> {
    let fields: Vec<_> = row.splitn(11, '\t').collect();
    if fields.len() != 11 {
        return Err("Invalid GPU sysfs row".into());
    }
    let vendor = fields[2];
    if !matches!(vendor, "NVIDIA" | "AMD" | "Intel") {
        return Err("Unknown GPU vendor".into());
    }
    let id = pci_id(fields[1]).ok_or("Invalid GPU PCI address")?;
    let product = fields[3].strip_prefix("0x").filter(|product| {
        product.len() == 4 && product.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let name = display_name(fields[10]).unwrap_or_else(|| match product {
        Some(product) => format!("{vendor} [{product}]"),
        None => vendor.to_owned(),
    });
    let problem = match fields[9] {
        "ok" => None,
        "denied" => Some(Problem::PermissionDenied),
        "failed" => Some(Problem::ToolFailed),
        _ => return Err("Invalid GPU metric status".into()),
    };
    Ok(Device {
        id,
        name,
        vendor: vendor.into(),
        utilization: (vendor == "AMD")
            .then(|| number(fields[4], 0., 100.))
            .flatten(),
        memory_used: (vendor == "AMD").then(|| bytes(fields[5], 1)).flatten(),
        memory_total: (vendor == "AMD").then(|| bytes(fields[6], 1)).flatten(),
        temperature: number(fields[7], -273_150., 250_000.).map(|value| value / 1000.),
        // AMD power1_* is emitted only for explicit CEM/OAM board_info,
        // never unidentified/APU SoC power. Intel's power*_max is a limit
        // and energy*_input a counter; neither is an instantaneous wattage.
        power: (vendor == "AMD")
            .then(|| number(fields[8], 0., 10_000_000_000.).map(|value| value / 1_000_000.))
            .flatten(),
        problem: if vendor == "NVIDIA" {
            nvidia_problem.or(problem)
        } else {
            problem
        },
    })
}

fn parse_nvidia(row: &str) -> Result<Device, String> {
    let fields = csv_fields(row).ok_or("Invalid NVIDIA CSV row")?;
    if fields.len() != 7 {
        return Err("Invalid NVIDIA CSV field count".into());
    }
    Ok(Device {
        id: pci_id(&fields[0]).ok_or("Invalid NVIDIA PCI address")?,
        name: display_name(&fields[1]).unwrap_or_else(|| "NVIDIA".into()),
        vendor: "NVIDIA".into(),
        utilization: number(&fields[2], 0., 100.),
        memory_used: bytes(&fields[3], 1024 * 1024),
        memory_total: bytes(&fields[4], 1024 * 1024),
        temperature: number(&fields[5], -273.15, 250.),
        power: number(&fields[6], 0., 10_000.),
        problem: None,
    })
}

fn merge_metrics(target: &mut Device, other: Device) {
    target.utilization = target.utilization.or(other.utilization);
    target.memory_used = target.memory_used.or(other.memory_used);
    target.memory_total = target.memory_total.or(other.memory_total);
    target.temperature = target.temperature.or(other.temperature);
    target.power = target.power.or(other.power);
}

fn has_metrics(device: &Device) -> bool {
    device.utilization.is_some()
        || device.memory_used.is_some()
        || device.memory_total.is_some()
        || device.temperature.is_some()
        || device.power.is_some()
}

fn validate_memory(device: &mut Device) {
    if device.memory_total == Some(0) {
        device.memory_total = None;
        device.memory_used = None;
    } else if let (Some(used), Some(total)) = (device.memory_used, device.memory_total)
        && used > total
    {
        device.memory_used = None;
    }
}

fn display_name(value: &str) -> Option<String> {
    let name: String = value
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect();
    let name = name.trim();
    (!name.is_empty() && !matches!(name, "N/A" | "[Not Supported]")).then(|| name.to_owned())
}

fn number(value: &str, min: f32, max: f32) -> Option<f32> {
    let value = value.trim().parse::<f32>().ok()?;
    (value.is_finite() && (min..=max).contains(&value)).then_some(value)
}

fn bytes(value: &str, scale: u64) -> Option<u64> {
    // Bound implausible/sentinel values as well as arithmetic overflow (16 TiB).
    value
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(scale)
        .filter(|value| *value <= 1 << 44)
}

fn pci_id(value: &str) -> Option<String> {
    let fields: Vec<_> = value.trim().split([':', '.']).collect();
    if fields.len() != 4
        || !matches!(fields[0].len(), 4 | 8)
        || fields[1].len() != 2
        || fields[2].len() != 2
        || fields[3].len() != 1
        || fields
            .iter()
            .any(|field| !field.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return None;
    }
    let part = |ix: usize| u32::from_str_radix(fields[ix], 16).ok();
    let (domain, bus, device, function) = (part(0)?, part(1)?, part(2)?, part(3)?);
    (domain <= 0xffff && device <= 0x1f && function <= 7)
        .then(|| format!("{domain:04x}:{bus:02x}:{device:02x}.{function:x}"))
}

/// A small single-record CSV reader: comma-containing names, escaped quotes,
/// whitespace outside quotes, and strict rejection of unclosed/misplaced quotes.
fn csv_fields(row: &str) -> Option<Vec<String>> {
    if row.len() > 2048 {
        return None;
    }
    let mut fields = Vec::new();
    let mut chars = row.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next()? {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    '"' => break,
                    c => field.push(c),
                }
            }
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                chars.next();
            }
            if chars.peek().is_some_and(|c| *c != ',') {
                return None;
            }
        } else {
            while chars.peek().is_some_and(|c| *c != ',') {
                let c = chars.next()?;
                if c == '"' {
                    return None;
                }
                field.push(c);
            }
        }
        fields.push(field.trim().to_owned());
        if fields.len() > 7 {
            return None;
        }
        if chars.next().is_none() {
            break;
        }
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(nvidia: &str, status: &str, sysfs: &str) -> String {
        format!(
            "@@shellrs-gpu-v1\n@@nvidia-begin\n{nvidia}\n@@nvidia-status={status}\n@@sysfs-begin\n{sysfs}\n@@sysfs-status=ok\n@@shellrs-gpu-end\n"
        )
    }

    #[test]
    fn quoted_csv_and_mib_have_correct_units() {
        let devices = parse(&sample(
            "00000000:01:00.0, \"NVIDIA A, \"\"Lab\"\"\", 12, 1024, 8192, 45, 75.25",
            "0",
            "",
        ))
        .unwrap();
        assert_eq!(devices.len(), 1);
        let gpu = &devices[0];
        assert_eq!(gpu.id, "0000:01:00.0");
        assert_eq!(gpu.name, "NVIDIA A, \"Lab\"");
        assert_eq!(gpu.memory_used, Some(1024 * 1024 * 1024));
        assert_eq!(gpu.memory_total, Some(8 * 1024 * 1024 * 1024));
        assert_eq!(gpu.utilization, Some(12.));
        assert_eq!(gpu.temperature, Some(45.));
        assert_eq!(gpu.power, Some(75.25));
        assert_eq!(gpu.problem, None);
    }

    #[test]
    fn unavailable_and_bad_numbers_are_unknown_not_zero() {
        for row in [
            "0000:01:00.0, Test, N/A, [Not Supported], N/A, N/A, N/A",
            "0000:01:00.0, Test, NaN, 18446744073709551615, -1, inf, -1",
            "0000:01:00.0, Test, 101, 16777217, 0, 251, 10001",
        ] {
            let gpu = &parse(&sample(row, "0", "")).unwrap()[0];
            assert!(!has_metrics(gpu), "{row}: {gpu:?}");
            assert_eq!(gpu.problem, Some(Problem::MetricsUnavailable));
        }
        let gpu = &parse(&sample("0000:01:00.0, Test, 0, 0, 100, 0, 0", "0", "")).unwrap()[0];
        assert_eq!(gpu.utilization, Some(0.));
        assert_eq!(gpu.memory_used, Some(0));
        assert_eq!(gpu.power, Some(0.));
    }

    #[test]
    fn inconsistent_memory_is_not_displayed_as_valid_usage() {
        let gpu = &parse(&sample("0000:01:00.0, Test, 1, 8193, 8192, 20, 1", "0", "")).unwrap()[0];
        assert_eq!(gpu.memory_used, None);
        assert_eq!(gpu.memory_total, Some(8 * 1024 * 1024 * 1024));
    }

    #[test]
    fn empty_is_distinct_from_missing_framing_permission_and_tool_failure() {
        assert_eq!(parse(&sample("", "missing", "")), Ok(vec![]));
        assert!(parse(&sample("", "6", "")).is_err());
        for output in [
            String::new(),
            "@@shellrs-gpu-v1\n".into(),
            sample("", "4", ""),
            sample("", "9", ""),
            sample("", "unavailable", ""),
            sample("", "missing", "").replace("@@sysfs-status=ok", "@@sysfs-status=denied"),
            sample("", "missing", "").replace("@@sysfs-status=ok", "@@sysfs-status=unavailable"),
            sample("", "missing", "").replace("@@shellrs-gpu-end", ""),
        ] {
            assert!(parse(&output).is_err(), "{output}");
        }
    }

    #[test]
    fn malformed_csv_identity_and_protocol_are_rejected() {
        for row in [
            "0000:01:00.0, \"Unclosed, 0, 0, 1, 2, 3",
            "0000:01:00.0, \"Name\"suffix, 0, 0, 1, 2, 3",
            "0000:01:00.0, Name, 0, 0, 1, 2",
            "0000:01:00.0, Name, 0, 0, 1, 2, 3, extra",
            "not-a-bus, Name, 0, 0, 1, 2, 3",
            "0000:01:20.0, Name, 0, 0, 1, 2, 3",
            "0000:01:00.8, Name, 0, 0, 1, 2, 3",
        ] {
            assert!(parse(&sample(row, "0", "")).is_err(), "{row}");
        }
        assert!(parse(&sample("", "0\n@@nvidia-status=0", "")).is_err());
        assert!(parse(&(sample("", "0", "") + "unframed")).is_err());
        assert!(parse(&"x".repeat(131_073)).is_err());
    }

    #[test]
    fn mixed_vendors_deduplicate_by_normalized_pci_identity() {
        let sysfs = "@@device\t0000:01:00.0\tNVIDIA\t0x2684\t\t\t\t45000\t\tok\t\n\
                     @@device\t0000:01:00.0\tNVIDIA\t0x2684\t\t\t\t45000\t\tok\t\n\
                     @@device\t0000:03:00.0\tAMD\t0x744c\t34\t1048576\t8589934592\t51250\t\tok\tRadeon\n\
                     @@device\t0000:00:02.0\tIntel\t0x1234\t\t\t\t40000\t14500000\tok\t";
        let devices = parse(&sample(
            "00000000:01:00.0, GeForce, 76, 1024, 8192, N/A, 99",
            "0",
            sysfs,
        ))
        .unwrap();
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].vendor, "Intel");
        assert_eq!(devices[0].utilization, None);
        assert_eq!(devices[0].power, None);
        assert_eq!(devices[1].name, "GeForce");
        assert_eq!(devices[1].temperature, Some(45.));
        assert_eq!(devices[1].utilization, Some(76.));
        assert_eq!(devices[2].memory_used, Some(1_048_576));
        assert_eq!(devices[2].temperature, Some(51.25));
        assert_eq!(devices[2].power, None);
    }

    #[test]
    fn known_devices_preserve_failure_reason_and_other_vendors() {
        let sysfs = "@@device\t0000:01:00.0\tNVIDIA\t0x2684\t\t\t\t\t\tok\t\n\
                     @@device\t0000:03:00.0\tAMD\t0x744c\t22\t\t\t\t\tok\t";
        for (status, problem) in [
            ("missing", Problem::MissingTool),
            ("4", Problem::PermissionDenied),
            ("9", Problem::ToolFailed),
            ("137", Problem::ToolFailed),
            ("unavailable", Problem::ToolFailed),
        ] {
            let devices = parse(&sample("not csv: vendor tool error", status, sysfs)).unwrap();
            assert_eq!(devices.len(), 2);
            assert_eq!(devices[0].problem, Some(problem));
            assert_eq!(devices[1].utilization, Some(22.));
        }
    }

    #[cfg(unix)]
    mod shell {
        use super::*;
        use std::fs;
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        struct Fixture(PathBuf);

        impl Fixture {
            fn new() -> Self {
                let root = std::env::temp_dir().join(format!(
                    "shellrs-gpu-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&root).unwrap();
                let this = Self(root);
                for directory in ["pci", "drm", "bin"] {
                    fs::create_dir(this.0.join(directory)).unwrap();
                }
                symlink("/bin/sh", this.0.join("bin/sh")).unwrap();
                // A fake timeout keeps tests independent of GNU coreutils on
                // macOS. The production command is tested with real timeout below.
                this.executable("timeout", "shift 3\nexec \"$@\"\n");
                symlink("/usr/bin/head", this.0.join("bin/head")).unwrap();
                this
            }

            fn write(&self, path: impl AsRef<Path>, contents: &str) {
                let path = self.0.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, contents).unwrap();
            }

            fn executable(&self, name: &str, body: &str) {
                let path = self.0.join("bin").join(name);
                fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }

            fn gpu(&self, id: &str, vendor: &str, hwmon: &str) {
                for (file, value) in [
                    ("class", "0x030000"),
                    ("vendor", vendor),
                    ("device", "0x1234"),
                    ("hwmon/hwmon0/name", hwmon),
                ] {
                    self.write(format!("pci/{id}/{file}"), &format!("{value}\n"));
                }
                self.write(format!("pci/{id}/uevent"), &format!("PCI_SLOT_NAME={id}\n"));
            }

            fn script(&self) -> String {
                command()
                    .replace("/sys/bus/pci/devices", self.0.join("pci").to_str().unwrap())
                    .replace("/sys/class/drm", self.0.join("drm").to_str().unwrap())
            }

            fn output(&self) -> String {
                let output =
                    crate::testing::sh(&self.script(), Some(self.0.join("bin").to_str().unwrap()));
                assert!(output.status.success(), "{:?}", output);
                String::from_utf8(output.stdout).unwrap()
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn command_is_one_safe_posix_shell_payload() {
            let cmd = command();
            assert_eq!(cmd.matches('\'').count(), 2);
            assert!(!cmd.contains('!'));
            assert!(!cmd.contains('\n'));
            assert!(crate::testing::sh_accepts(&cmd));
            assert_eq!(parse(&Fixture::new().output()), Ok(vec![]));
        }

        #[test]
        fn reads_fake_sysfs_deduplicates_drm_and_skips_non_gpus() {
            let fixture = Fixture::new();
            fixture.gpu("0000:03:00.0", "0x1002", "amdgpu");
            fixture.write("pci/0000:03:00.0/gpu_busy_percent", "37\n");
            fixture.write("pci/0000:03:00.0/mem_info_vram_used", "1048576\n");
            fixture.write("pci/0000:03:00.0/mem_info_vram_total", "8589934592\n");
            fixture.write("pci/0000:03:00.0/hwmon/hwmon0/temp1_input", "51250\n");
            fixture.write("pci/0000:03:00.0/hwmon/hwmon0/power1_average", "80000000\n");
            fixture.write(
                "pci/0000:03:00.0/product_name",
                "Radeon $(exit 17); quoted \"name\"\n",
            );
            fs::create_dir(fixture.0.join("drm/card5")).unwrap();
            symlink(
                fixture.0.join("pci/0000:03:00.0"),
                fixture.0.join("drm/card5/device"),
            )
            .unwrap();
            fixture.gpu("0000:03:00.1", "0x1002", "amdgpu");
            fixture.write("pci/0000:03:00.1/class", "0x040300\n");
            fixture.gpu("0000:04:00.0", "0x1234", "other");
            fixture.gpu("0000:00:02.0", "0x8086", "i915");
            let devices = parse(&fixture.output()).unwrap();
            assert_eq!(devices.len(), 2);
            assert_eq!(devices[0].utilization, None);
            assert_eq!(devices[0].problem, Some(Problem::MetricsUnavailable));
            let gpu = &devices[1];
            assert_eq!(gpu.utilization, Some(37.));
            assert_eq!(gpu.temperature, Some(51.25));
            assert_eq!(gpu.memory_used, Some(1_048_576));
            assert_eq!(
                gpu.power, None,
                "Unidentified/APU SoC power is not GPU power"
            );
            assert_eq!(gpu.name, "Radeon $(exit 17); quoted \"name\"");
            fixture.write("pci/0000:03:00.0/board_info", "type : cem\n");
            assert_eq!(parse(&fixture.output()).unwrap()[1].power, Some(80.));
        }

        #[test]
        fn fake_nvidia_query_and_failures_are_framed_and_preserve_identity() {
            let fixture = Fixture::new();
            fixture.gpu("0000:01:00.0", "0x10de", "nouveau");
            fixture.executable("nvidia-smi", "test \"$#\" = 2 || exit 2\ntest \"$2\" = --format=csv,noheader,nounits || exit 2\nprintf '%s\\n' '00000000:01:00.0, Test GPU, 75, 1, 8192, 44, 60'\n");
            let devices = parse(&fixture.output()).unwrap();
            assert_eq!(devices.len(), 1);
            assert_eq!(devices[0].utilization, Some(75.));
            for (body, problem) in [
                ("echo vendor-error >&2; exit 4\n", Problem::PermissionDenied),
                ("echo vendor-error; exit 9\n", Problem::ToolFailed),
            ] {
                fixture.executable("nvidia-smi", body);
                assert_eq!(parse(&fixture.output()).unwrap()[0].problem, Some(problem));
            }
            fs::remove_file(fixture.0.join("bin/timeout")).unwrap();
            assert_eq!(
                parse(&fixture.output()).unwrap()[0].problem,
                Some(Problem::ToolFailed)
            );
            fs::remove_file(fixture.0.join("bin/nvidia-smi")).unwrap();
            assert_eq!(
                parse(&fixture.output()).unwrap()[0].problem,
                Some(Problem::MissingTool)
            );
        }

        #[test]
        fn intel_uses_driver_specific_gpu_temperature_not_power_limits() {
            let fixture = Fixture::new();
            fixture.gpu("0000:00:02.0", "0x8086", "i915");
            fixture.write("pci/0000:00:02.0/hwmon/hwmon0/temp1_input", "42000\n");
            fixture.write("pci/0000:00:02.0/hwmon/hwmon0/power1_max", "180000000\n");
            fixture.gpu("0000:04:00.0", "0x8086", "xe");
            fixture.write("pci/0000:04:00.0/hwmon/hwmon0/temp1_input", "99000\n");
            fixture.write("pci/0000:04:00.0/hwmon/hwmon0/temp2_input", "51000\n");
            fixture.write("pci/0000:04:00.0/hwmon/hwmon0/temp3_input", "60000\n");
            fixture.write("pci/0000:04:00.0/hwmon/hwmon0/energy1_input", "1000000\n");
            let devices = parse(&fixture.output()).unwrap();
            assert_eq!(devices[0].temperature, Some(42.));
            assert_eq!(devices[1].temperature, Some(51.));
            for device in devices {
                assert_eq!(device.power, None);
                assert_eq!(device.utilization, None);
                assert_eq!(device.memory_total, None);
            }
        }

        #[test]
        fn unreadable_optional_metric_is_a_device_problem_not_no_gpu() {
            let fixture = Fixture::new();
            fixture.gpu("0000:03:00.0", "0x1002", "amdgpu");
            let path = fixture.0.join("pci/0000:03:00.0/gpu_busy_percent");
            fs::write(&path, "10\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            // A root test runner can still read mode 000. Do not pretend that
            // such a runner exercises permission denial.
            if fs::read_to_string(&path).is_err() {
                let devices = parse(&fixture.output()).unwrap();
                assert_eq!(devices.len(), 1);
                assert_eq!(devices[0].problem, Some(Problem::PermissionDenied));
                assert_eq!(devices[0].utilization, None);
            }
        }

        #[test]
        fn unreadable_discovery_is_not_reported_as_no_devices() {
            let fixture = Fixture::new();
            fixture.gpu("0000:03:00.0", "0x1002", "amdgpu");
            let path = fixture.0.join("pci/0000:03:00.0");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read_dir(&path).is_err() {
                assert!(parse(&fixture.output()).is_err());
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }

        #[test]
        fn output_limit_rejects_truncated_vendor_data() {
            let fixture = Fixture::new();
            fixture.executable("nvidia-smi", "i=0; while test \"$i\" -lt 1000; do printf '%s\\n' '0000:01:00.0, Test GPU, 75, 1, 8192, 44, 60'; i=$((i + 1)); done\n");
            let output = fixture.output();
            assert!(output.len() < 17_000);
            assert!(parse(&output).is_err());
        }

        #[test]
        fn real_timeout_kills_a_stuck_fake_vendor_tool() {
            let fixture = Fixture::new();
            // Linux CI checks the actual timeout path; macOS has no coreutils
            // timeout by default, so its other fixture tests use the fake above.
            if !Path::new("/usr/bin/timeout").exists() {
                return;
            }
            fs::remove_file(fixture.0.join("bin/timeout")).unwrap();
            symlink("/usr/bin/timeout", fixture.0.join("bin/timeout")).unwrap();
            symlink("/bin/sleep", fixture.0.join("bin/sleep")).unwrap();
            fixture.executable("nvidia-smi", "exec sleep 10\n");
            fixture.gpu("0000:01:00.0", "0x10de", "nouveau");
            let start = std::time::Instant::now();
            let devices = parse(&fixture.output()).unwrap();
            assert!(start.elapsed() < std::time::Duration::from_secs(6));
            assert_eq!(devices[0].problem, Some(Problem::ToolFailed));
        }
    }
}
