//! Which video encoders actually work on this machine.
//!
//! `ffmpeg -encoders` lists what ffmpeg was *built* with, which is every
//! hardware encoder under the sun on a distribution build — the AMD laptop
//! in issue #15 had `h264_nvenc` "available" by that test and failed with
//! "Cannot load libcuda.so.1" the moment it was used. The only honest check
//! is to encode a frame. One tiny frame takes well under a second and only
//! happens when the configured encoder is in doubt.

use tracing::{info, warn};

/// In order of preference. NVENC and VAAPI are the two hardware paths this
/// project tunes for; libx264 works everywhere and is what a laptop without
/// either falls back to.
pub const CANDIDATES: [&str; 3] = ["h264_nvenc", "h264_vaapi", "libx264"];

/// The render node VAAPI encodes on, once a probe has found one that works.
static VAAPI_DEVICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The render node to give ffmpeg's `-vaapi_device`: `USCREEN_VAAPI_DEVICE` if
/// set, else the one a probe found working, else the old fixed guess. Both the
/// test frame and the real encode use this, so they cannot disagree.
pub fn vaapi_device() -> String {
    std::env::var("USCREEN_VAAPI_DEVICE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| VAAPI_DEVICE.lock().ok().and_then(|g| g.clone()))
        .unwrap_or_else(|| "/dev/dri/renderD128".to_string())
}

/// Render nodes worth trying for VAAPI, the GPUs that can run it first. The
/// fixed `renderD128` is NVIDIA's on many machines (it is the first GPU
/// registered, not the one with VAAPI), so a user who chose h264_vaapi had
/// the test frame fail there and was switched back to NVENC.
fn render_nodes() -> Vec<String> {
    let mut nodes: Vec<(u8, String)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/dev/dri") {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("renderD") {
                continue;
            }
            let driver = std::fs::read_link(format!("/sys/class/drm/{}/device/driver", name))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_default();
            let rank = match driver.as_str() {
                "amdgpu" | "radeon" | "i915" | "xe" => 0,
                "nvidia" | "nouveau" => 2,
                _ => 1,
            };
            nodes.push((rank, format!("/dev/dri/{}", name)));
        }
    }
    nodes.sort();
    nodes.into_iter().map(|(_, path)| path).collect()
}

/// Encode one 256x144 frame with `encoder`, bounded to a few seconds.
/// The error is ffmpeg's own last line, which is what a bug report needs —
/// "cannot encode" alone does not say whether the driver, the device node
/// or the ffmpeg build is at fault.
///
/// VAAPI is tried on each render node in turn, and the one that works is
/// remembered for the real encode.
pub async fn probe(encoder: &str) -> Result<(), String> {
    if !encoder.contains("vaapi") {
        return probe_on(encoder, None).await;
    }
    let mut nodes: Vec<String> = std::env::var("USCREEN_VAAPI_DEVICE")
        .ok()
        .filter(|s| !s.is_empty())
        .into_iter()
        .collect();
    if nodes.is_empty() {
        nodes = render_nodes();
    }
    if nodes.is_empty() {
        nodes.push("/dev/dri/renderD128".to_string());
    }
    let mut errors = Vec::new();
    for node in &nodes {
        match probe_on(encoder, Some(node)).await {
            Ok(()) => {
                info!("VAAPI works on {}", node);
                if let Ok(mut g) = VAAPI_DEVICE.lock() {
                    *g = Some(node.clone());
                }
                return Ok(());
            }
            Err(e) => errors.push(format!("{}: {}", node, e)),
        }
    }
    Err(errors.join("; "))
}

async fn probe_on(encoder: &str, vaapi_device: Option<&str>) -> Result<(), String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "color=size=256x144:rate=1",
        // NVENC refuses anything smaller than about 145x49, and lavfi's
        // default is RGB, which no hardware encoder takes.
        "-pix_fmt",
        "nv12",
        "-frames:v",
        "1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(dev) = vaapi_device {
        args.extend([
            "-vaapi_device".to_string(),
            dev.to_string(),
            "-vf".to_string(),
            "format=nv12,hwupload".to_string(),
        ]);
    }
    args.extend([
        "-c:v".to_string(),
        encoder.to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ]);
    let run = tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(8), run).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => {
            // The first line is the cause ("Cannot load libcuda.so.1");
            // the last is ffmpeg's generic "Nothing was written" epilogue.
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("ffmpeg failed without a message");
            // Drop the "[h264_amf @ 0x55bd…] " context prefixes.
            let mut msg = first;
            while let Some(rest) = msg.strip_prefix('[').and_then(|r| r.split_once("] ")) {
                msg = rest.1;
            }
            Err(msg.to_string())
        }
        Ok(Err(e)) => Err(format!("could not run ffmpeg: {}", e)),
        Err(_) => Err("no frame within 8 s".to_string()),
    }
}

pub async fn works(encoder: &str) -> bool {
    probe(encoder).await.is_ok()
}

/// `configured` if it works, otherwise the first candidate that does — with
/// a log line saying why the configuration was overridden. `None` only when
/// nothing encodes at all, in which case the caller should say so and stop.
pub async fn resolve(configured: &str) -> Option<String> {
    // The old gstreamer-style alias.
    let configured = if configured == "vaapih264enc" { "h264_vaapi" } else { configured };
    let first = match probe(configured).await {
        Ok(()) => return Some(configured.to_string()),
        Err(e) => e,
    };
    // Once more before overriding what the user chose: at login a hybrid
    // laptop's discrete GPU may still be waking up, or its driver still
    // loading, and replacing the encoder is saved to the config.
    info!("Encoder {} failed a test frame ({}), trying once more", configured, first);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let reason = match probe(configured).await {
        Ok(()) => return Some(configured.to_string()),
        Err(e) => e,
    };
    let mut fallback = None;
    for e in CANDIDATES.into_iter().filter(|e| *e != configured) {
        if works(e).await {
            fallback = Some(e);
            break;
        }
    }
    match fallback {
        Some(e) => {
            warn!(
                "Encoder {} does not work on this machine ({}) — using {} instead. \
                 (Set `encoder` in the config or the GUI to keep a different choice.)",
                configured, reason, e
            );
            Some(e.to_string())
        }
        None => {
            info!("No H.264 encoder works here: tried {}", CANDIDATES.join(", "));
            None
        }
    }
}
