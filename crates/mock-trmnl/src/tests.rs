use std::io::{Read, Write};
use std::net::TcpStream;

use super::*;

/// A minimal HTTP/1.1 client: (status, headers, body).
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 10.0.2.2:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "Content-Length: 0\r\n\r\n";
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&resp[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, resp[split + 4..].to_vec())
}

fn get_json(port: u16, path: &str, headers: &[(&str, &str)]) -> Value {
    let (code, _, body) = http(port, "GET", path, headers);
    assert_eq!(code, 200, "{path}");
    serde_json::from_slice(&body).unwrap()
}

fn started(panel: Panel) -> (MockServer, u16) {
    let m = MockServer::new(panel);
    let port = m.start(0).unwrap().port();
    (m, port)
}

fn png_of(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb(f(x, y)));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

#[test]
fn setup_registers_the_device() {
    let (m, port) = started(Panel::Og);
    let v = get_json(port, "/api/setup", &[("ID", "7C:DF:A1:00:00:01")]);
    assert_eq!(v["api_key"], "sim-test-api-key");
    assert_eq!(v["friendly_id"], "SIMTST");
    assert_eq!(v["image_url"], format!("http://10.0.2.2:{port}/images/default.bmp"));
    m.state().registered = false;
    let (code, _, _) = http(port, "GET", "/api/setup", &[]);
    assert_eq!(code, 404);
}

#[test]
fn display_points_at_the_current_image_with_a_cache_filename() {
    let (m, port) = started(Panel::Og);
    let v = get_json(port, "/api/display", &[]);
    assert_eq!(v["status"], 0);
    assert_eq!(v["image_url"], format!("http://10.0.2.2:{port}/images/default.bmp"));
    assert_eq!(v["refresh_rate"], 900);
    assert_eq!(v["update_firmware"], false);
    assert_eq!(v["reset_firmware"], false);
    assert_eq!(v["special_function"], "sleep");
    let f = v["filename"].as_str().unwrap();
    assert!(f.starts_with("plugin-") && f.len() > 14 && f.as_bytes()[13] == b'-', "{f}");

    let (code, head, body) = http(port, "GET", "/images/default.bmp", &[]);
    assert_eq!(code, 200);
    assert!(head.contains("Content-Type: image/bmp"), "{head}");
    assert!(head.contains(&format!("Content-Length: {}", body.len())), "{head}");
    assert_eq!(&body[..2], b"BM");

    // A new version of an image gets a later filename (so the device downloads it again).
    let png = png_of(800, 480, |x, _| if x < 400 { [0; 3] } else { [255; 3] });
    m.add_image("half", &png, ConvertOptions::default()).unwrap();
    let first = m.state().image("half").unwrap().filename.clone();
    m.add_image("half", &png, ConvertOptions::default()).unwrap();
    let second = m.state().image("half").unwrap().filename.clone();
    assert_eq!(first[..14], second[..14], "same plugin id");
    assert!(second > first);

    m.state().set_current("half").unwrap();
    m.state().refresh_rate = 300;
    let v = get_json(port, "/api/display", &[]);
    assert_eq!(v["image_url"], format!("http://10.0.2.2:{port}/images/half.bmp"));
    assert_eq!(v["filename"], second.as_str());
    assert_eq!(v["refresh_rate"], 300);
}

#[test]
fn queue_and_extra_fields_shape_the_answer() {
    let (m, port) = started(Panel::Og);
    m.state().extra.insert("maximum_compatibility".into(), json!(true));
    let mut q = Map::new();
    q.insert("update_firmware".into(), json!(true));
    q.insert("firmware_url".into(), json!("http://10.0.2.2/firmware.bin"));
    m.state().enqueue(q);
    let v = get_json(port, "/api/display", &[]);
    assert_eq!((v["update_firmware"].clone(), v["maximum_compatibility"].clone()), (json!(true), json!(true)));
    assert_eq!(v["firmware_url"], "http://10.0.2.2/firmware.bin");
    let v = get_json(port, "/api/display", &[]);
    assert_eq!(v["update_firmware"], false, "one-shot");
    assert_eq!(v["maximum_compatibility"], true, "persistent");
}

#[test]
fn playlist_advances_per_request_and_rewinds_on_double_click() {
    let (m, port) = started(Panel::Og);
    for n in ["a", "b", "c"] {
        m.add_image(n, &png_of(8, 8, |_, _| [0; 3]), ConvertOptions::default()).unwrap();
    }
    {
        let mut st = m.state();
        st.playlist = vec!["a".into(), "b".into(), "c".into()];
        st.auto_advance = true;
        st.special_function = "rewind".into();
    }
    let image = |v: Value| v["image_url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let seq: Vec<String> = (0..4).map(|_| image(get_json(port, "/api/display", &[]))).collect();
    assert_eq!(seq, ["a.bmp", "b.bmp", "c.bmp", "a.bmp"]);
    // Double-click with the rewind function: back one, and the action is echoed.
    let v = get_json(port, "/api/display", &[("special_function", "true")]);
    assert_eq!((image(v.clone()), v["action"].clone()), ("c.bmp".to_string(), json!("rewind")));
}

#[test]
fn identify_double_click_shows_the_friendly_id() {
    let (m, port) = started(Panel::X);
    m.state().special_function = "identify".into();
    let v = get_json(port, "/api/display", &[("special_function", "true")]);
    assert_eq!(v["action"], "identify");
    let url = v["image_url"].as_str().unwrap();
    assert!(url.ends_with("/images/_identify.png"), "{url}");
    let (code, _, body) = http(port, "GET", "/images/_identify.png", &[]);
    assert_eq!(code, 200);
    assert_eq!(image::load_from_memory(&body).unwrap().width(), 1872);
}

#[test]
fn files_log_and_404() {
    let (m, port) = started(Panel::Bwry);
    let url = m.set_file("/firmware.bin", FileSource::Bytes(Arc::new(vec![0xe9; 70_000])));
    assert_eq!(url, format!("http://10.0.2.2:{port}/firmware.bin"));
    let (code, head, body) = http(port, "GET", "/firmware.bin", &[]);
    assert_eq!((code, body.len()), (200, 70_000));
    assert!(head.contains("Content-Length: 70000") && !head.contains("chunked"), "{head}");
    assert_eq!(http(port, "GET", "/nope", &[]).0, 404);
    assert_eq!(http(port, "POST", "/api/log", &[]).0, 200);
    let st = m.state();
    let paths: Vec<&str> = st.requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/firmware.bin", "/nope", "/api/log"]);
    assert_eq!(st.requests[1].status, 404);
    assert_eq!(st.requests_since(2).count(), 1);
}

#[test]
fn headers_are_recorded_case_insensitively_with_sim_time() {
    let (m, port) = started(Panel::Og);
    m.set_clock(|| 42_000_000_000);
    get_json(port, "/api/display", &[("update-source", "timer"), ("Battery-Voltage", "4.10")]);
    let st = m.state();
    let r = st.requests.back().unwrap();
    assert_eq!(r.header("Update-Source"), Some("timer"));
    assert_eq!(r.header("battery-voltage"), Some("4.10"));
    assert_eq!(r.sim_time_ns, Some(42_000_000_000));
    assert!(r.summary.starts_with("image default"), "{}", r.summary);
}

#[test]
fn bwry_images_are_palette_pngs() {
    let (m, port) = started(Panel::Bwry);
    let png = png_of(800, 480, |x, _| [[0, 0, 0], [255, 255, 255], [255, 255, 0], [255, 0, 0]][x as usize / 200]);
    m.add_image("bars", &png, ConvertOptions { dither: false, fit: Fit::Contain }).unwrap();
    m.state().set_current("bars").unwrap();
    let v = get_json(port, "/api/display", &[]);
    assert!(v["image_url"].as_str().unwrap().ends_with("/images/bars.png"));
    let (_, head, body) = http(port, "GET", "/images/bars.png", &[]);
    assert!(head.contains("Content-Type: image/png"));
    assert_eq!((body[24], body[25]), (2, 3));
}

#[test]
fn stop_and_restart() {
    let (m, port) = started(Panel::Og);
    assert_eq!(m.device_url(), Some(format!("http://10.0.2.2:{port}")));
    m.stop();
    assert_eq!(m.device_url(), None);
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    let port2 = m.start(0).unwrap().port();
    assert_eq!(get_json(port2, "/api/display", &[])["status"], 0);
}

#[test]
fn names_are_url_safe_and_unique() {
    let m = MockServer::new(Panel::Og);
    let st = m.state();
    assert_eq!(name_from_file(&st, "/tmp/My Photo.JPG"), "My_Photo");
    assert_eq!(name_from_file(&st, "default.png"), "default_2");
    assert_eq!(sanitize("__x"), "x");
}
