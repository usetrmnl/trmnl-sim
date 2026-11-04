//! Released firmware, from the public list the TRMNL web flasher uses
//! (<https://trmnl.com/api-docs#tag/flash-firmwares>): which builds exist per device model, and
//! downloading one (its merged image and ELF) to a directory per board.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

const CATALOG_URL: &str = "https://trmnl.com/api/firmware/flash";

/// The API's device model (`keyname`) to the PlatformIO env that builds it, from `S3_PREFIXES`
/// in the firmware's `.github/workflows/publish-release-firmware.yml` (a board's S3 prefix ends
/// in its keyname, except that the OG is `trmnl_og`, the BWRY `trmnl_bwry`). A model the
/// simulator has no board for, or a new one, is listed but can't be chosen.
const MODELS: &[(&str, &str)] = &[
    ("trmnl", "trmnl"),
    ("trmnl_4clr", "trmnl_4clr"),
    ("trmnl_x", "TRMNL_X"),
    ("seeed_OG_DIY_Kit", "TRMNL_7inch5_OG_DIY_Kit"),
    ("seeed_xiao_esp32c3", "seeed_xiao_esp32c3"),
    ("seeed_E1001", "seeed_reTerminal_E1001"),
    ("seeed_E1002", "seeed_reTerminal_E1002"),
    ("seeed_E1003", "TRMNL_X_E1003"),
    ("seeed_E1004", "seeed_reTerminal_E1004"),
    ("seeed_sticky", "seeed_sticky"),
    ("xteink_x4", "xteink_x4"),
    ("xteink_x3", "xteink_x3"),
    ("m5_paper_s3", "TRMNL_X_PAPERS3"),
    ("lilygo_t5pro", "TRMNL_X_LILYGO_T5PRO"),
];

/// Variant builds are published under their base board's model, with this suffix on the version
/// (`VARIANT_SUFFIXES` in the same workflow): (model, version suffix, env).
const VARIANTS: &[(&str, &str, &str)] = &[("xteink_x4", "-push-to-refresh", "xteink_x4_pwr_btn")];

/// Every env [`env_of`] can return.
pub fn release_envs() -> impl Iterator<Item = &'static str> {
    MODELS.iter().map(|m| m.1).chain(VARIANTS.iter().map(|v| v.2))
}

/// The PlatformIO env that built `version` of the model `keyname`.
pub fn env_of(keyname: &str, version: &str) -> Option<&'static str> {
    let variant = VARIANTS.iter().find(|(k, suffix, _)| *k == keyname && version.ends_with(suffix));
    variant.map(|v| v.2).or_else(|| MODELS.iter().find(|m| m.0 == keyname).map(|m| m.1))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub keyname: String,
    pub label: String,
    /// Newest first, as the API lists them.
    pub versions: Vec<Version>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub version: String,
    pub url: String,
    /// The simulator binds its HLE to the ELF's symbols, so a build without one can't run.
    pub elf_url: Option<String>,
}

/// The models in the API's response. Entries missing a required field are skipped, so one odd
/// row doesn't take away the rest.
pub fn parse_catalog(json: &[u8]) -> Result<Vec<Model>, String> {
    let v: Value = serde_json::from_slice(json).map_err(|e| format!("unexpected response: {e}"))?;
    let models = v["data"]["models"].as_array().ok_or("unexpected response: no data.models")?;
    let version = |v: &Value| {
        Some(Version {
            version: v["version"].as_str()?.into(),
            url: v["url"].as_str()?.into(),
            elf_url: v["elf_url"].as_str().map(Into::into),
        })
    };
    let model = |m: &Value| {
        Some(Model {
            keyname: m["keyname"].as_str()?.into(),
            label: m["label"].as_str()?.into(),
            versions: m["versions"].as_array()?.iter().filter_map(version).collect(),
        })
    };
    Ok(models.iter().filter_map(model).collect())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
        .into()
}

/// Blocks: call it off the UI thread.
pub fn fetch_catalog() -> Result<Vec<Model>, String> {
    let body = agent()
        .get(CATALOG_URL)
        .call()
        .and_then(|mut r| r.body_mut().read_to_vec())
        .map_err(|e| format!("{CATALOG_URL}: {e}"))?;
    parse_catalog(&body)
}

/// Where a release lives locally. One directory per board, not per version, so an OTA update to
/// another downloaded version of the board finds its ELF.
#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseFiles {
    pub bin: PathBuf,
    pub elf: PathBuf,
}

impl ReleaseFiles {
    /// `FW1.8.18-trmnl.bin`, named like the firmware's own versioned copies. The version is the
    /// API's free text (`CrossPoint Reader 1.0.0`), so only safe characters of it are kept.
    pub fn new(root: &Path, env: &str, version: &str) -> Self {
        let safe = |c: char| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' };
        let stem = format!("{}-{env}", version.chars().map(safe).collect::<String>());
        let dir = root.join(env);
        ReleaseFiles { bin: dir.join(format!("{stem}.bin")), elf: dir.join(format!("{stem}.elf")) }
    }

    pub fn downloaded(&self) -> bool {
        self.bin.is_file() && self.elf.is_file()
    }
}

/// How far a download is: the file, bytes so far and the file's size if the server said.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub file: String,
    pub done: u64,
    pub total: Option<u64>,
}

/// Download the image and the ELF. Nothing lands under its final name until both are complete,
/// so an interrupted download is never mistaken for a release, nor an image paired with another
/// build's ELF. Blocks: call it off the UI thread.
pub fn download(
    version: &Version,
    files: &ReleaseFiles,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Result<(), String> {
    let elf_url = version.elf_url.as_deref().ok_or("this build has no ELF")?;
    let dir = files.bin.parent().ok_or("no directory for the download")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let part = |p: &Path| p.with_extension("part");
    let result = fetch_to(&version.url, &part(&files.bin), cancel, &mut progress)
        .and_then(|()| fetch_to(elf_url, &part_elf(&files.elf), cancel, &mut progress))
        .and_then(|()| {
            std::fs::rename(part_elf(&files.elf), &files.elf)
                .and_then(|()| std::fs::rename(part(&files.bin), &files.bin))
                .map_err(|e| format!("{}: {e}", dir.display()))
        });
    if result.is_err() {
        let _ = std::fs::remove_file(part(&files.bin));
        let _ = std::fs::remove_file(part_elf(&files.elf));
    }
    result
}

/// The ELF's temporary name, distinct from the image's (same stem).
fn part_elf(elf: &Path) -> PathBuf {
    elf.with_extension("elf.part")
}

fn fetch_to(url: &str, path: &Path, cancel: &AtomicBool, progress: &mut impl FnMut(Progress)) -> Result<(), String> {
    // Some of the API's URLs have spaces in them (`.../Xteink Official.bin`).
    let response = agent().get(url.replace(' ', "%20")).call().map_err(|e| format!("{url}: {e}"))?;
    let total = response.body().content_length();
    let file = url.rsplit('/').next().unwrap_or(url).to_string();
    let mut reader = response.into_body().into_reader();
    let mut out = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; 64 * 1024];
    let mut done = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", path.display()))?;
        done += n as u64;
        progress(Progress { file: file.clone(), done, total });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_the_api_response() {
        let json = br#"{"data":{"models":[
            {"keyname":"trmnl","chipFamily":"ESP32-C3","label":"TRMNL OG (native)","versions":[
                {"version":"FW1.8.18","url":"https://x/a.bin","elf_url":"https://x/a.elf"},
                {"version":"FW1.8.17","url":"https://x/b.bin"},
                {"url":"https://x/no-version.bin"}]},
            {"label":"no keyname","versions":[]}]}}"#;
        let models = parse_catalog(json).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].label, "TRMNL OG (native)");
        assert_eq!(models[0].versions.len(), 2);
        assert_eq!(models[0].versions[0].elf_url.as_deref(), Some("https://x/a.elf"));
        assert_eq!(models[0].versions[1].elf_url, None);
        assert!(parse_catalog(b"<html>").is_err());
        assert!(parse_catalog(b"{}").is_err());
    }

    #[test]
    fn the_board_comes_from_the_model_and_the_version() {
        assert_eq!(env_of("seeed_E1001", "FW1.8.18"), Some("seeed_reTerminal_E1001"));
        assert_eq!(env_of("xteink_x4", "FW1.8.18"), Some("xteink_x4"));
        assert_eq!(env_of("xteink_x4", "FW1.8.18-push-to-refresh"), Some("xteink_x4_pwr_btn"));
        assert_eq!(env_of("xteink_x3", "FW1.8.18-push-to-refresh"), Some("xteink_x3"));
        assert_eq!(env_of("new_model", "FW1.8.18"), None);
    }

    #[test]
    fn a_version_cannot_leave_the_boards_directory() {
        let f = ReleaseFiles::new(Path::new("/r"), "trmnl", "../../evil name");
        assert_eq!(f.bin, Path::new("/r/trmnl/.._.._evil_name-trmnl.bin"));
        assert_eq!(f.elf, Path::new("/r/trmnl/.._.._evil_name-trmnl.elf"));
        assert_eq!(
            ReleaseFiles::new(Path::new("/r"), "trmnl", "FW1.8.18").bin,
            Path::new("/r/trmnl/FW1.8.18-trmnl.bin")
        );
    }

    /// Answers each request with the response for its path.
    fn serve(routes: &'static [(&'static str, &'static str)]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("");
                let response = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map_or("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", |r| r.1);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    const BIN: &str = "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbin";
    const ELF: &str = "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nelf";
    const SHORT: &str = "HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nel";

    fn try_download(name: &str, elf_path: &str) -> (Result<(), String>, ReleaseFiles, Vec<Progress>) {
        static ROUTES: &[(&str, &str)] = &[("/My%20FW.bin", BIN), ("/good.elf", ELF), ("/short.elf", SHORT)];
        let base = serve(ROUTES);
        let root = std::env::temp_dir().join(format!("sim-ui-release-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = ReleaseFiles::new(&root, "trmnl", "FW1");
        let version = Version {
            version: "FW1".into(),
            url: format!("{base}/My FW.bin"),
            elf_url: Some(format!("{base}{elf_path}")),
        };
        let mut seen = Vec::new();
        let r = download(&version, &files, &AtomicBool::new(false), |p| seen.push(p));
        (r, files, seen)
    }

    #[test]
    fn downloads_the_image_and_the_elf() {
        let (r, files, seen) = try_download("ok", "/good.elf");
        r.unwrap();
        assert!(files.downloaded());
        assert_eq!(std::fs::read(&files.bin).unwrap(), b"bin");
        assert_eq!(std::fs::read(&files.elf).unwrap(), b"elf");
        let last = seen.last().unwrap();
        assert_eq!((last.file.as_str(), last.done, last.total), ("good.elf", 3, Some(3)));
    }

    /// A failed or cut-off ELF leaves nothing behind that later looks like a downloaded release.
    #[test]
    fn a_failed_download_leaves_nothing() {
        for (name, elf) in [("missing", "/missing.elf"), ("short", "/short.elf")] {
            let (r, files, _) = try_download(name, elf);
            assert!(r.is_err(), "{name}");
            let dir = files.bin.parent().unwrap();
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0, "{name}");
        }
    }

    #[test]
    fn cancelling_stops_the_download() {
        let root = std::env::temp_dir().join(format!("sim-ui-release-{}-cancel", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = ReleaseFiles::new(&root, "trmnl", "FW1");
        let base = serve(&[("/a.bin", BIN), ("/a.elf", ELF)]);
        let version =
            Version { version: "FW1".into(), url: format!("{base}/a.bin"), elf_url: Some(format!("{base}/a.elf")) };
        assert_eq!(download(&version, &files, &AtomicBool::new(true), |_| ()), Err("cancelled".into()));
        assert!(!files.downloaded());
    }

    /// The live list only names models this table knows; a new one needs a row.
    #[test]
    #[ignore = "uses the network"]
    fn live_models_are_known() {
        for m in fetch_catalog().unwrap() {
            assert!(env_of(&m.keyname, "FW1").is_some(), "{} ({}) has no env", m.keyname, m.label);
        }
    }
}
