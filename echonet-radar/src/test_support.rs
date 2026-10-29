//! Fixtures shared by the app's tests.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::SystemTime;

use echonet_radar_core::{
    ObjectKey, ObjectSnapshot, PropertySnapshot, RadarSnapshot, SessionStatus,
};

/// Every text shape a frame drew, in paint order, with the colour it used.
pub fn texts(output: &mut egui::FullOutput) -> Vec<(String, egui::Color32)> {
    // A frame the test does not render leaves glyph deltas behind, and
    // dropping them panics while a failing assertion unwinds.
    output.textures_delta.clear();
    let mut collected = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, &mut collected);
    }
    collected
}

/// Every text shape a frame drew, ignoring colour.
pub fn drawn_text(output: &mut egui::FullOutput) -> Vec<String> {
    texts(output)
        .into_iter()
        .map(|(text, _color)| text)
        .collect()
}

fn collect(
    shape: &egui::Shape,
    collected: &mut Vec<(String, egui::Color32)>,
) {
    match shape {
        egui::Shape::Text(text) => {
            collected.push((text.galley.text().to_owned(), text_color(text)));
        },
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect(shape, collected);
            }
        },
        _ => {},
    }
}

fn text_color(text: &egui::epaint::TextShape) -> egui::Color32 {
    match text
        .galley
        .job
        .sections
        .first()
        .map(|section| section.format.color)
    {
        Some(color) if color != egui::Color32::PLACEHOLDER => color,
        _ => text.override_text_color.unwrap_or(text.fallback_color),
    }
}

/// Build an `ObjectKey` without naming the `echonet-lite` EOJ type, which the
/// app is not allowed to depend on. The instance list starts with its count.
pub fn key(
    ip: Ipv4Addr,
    class_code: u16,
) -> ObjectKey {
    let mut eojs = echonet_radar_core::parse_instance_list(&[
        1,
        u8::try_from(class_code >> 8).expect("class group"),
        u8::try_from(class_code & 0x00FF).expect("class code"),
        1,
    ])
    .expect("a one-item instance list parses");
    ObjectKey {
        ip,
        eoj: eojs.remove(0),
    }
}

pub fn property(
    edt: &[u8],
    at: SystemTime,
) -> PropertySnapshot {
    PropertySnapshot {
        raw_edt: edt.to_vec(),
        text: String::from("value"),
        at,
    }
}

/// A snapshot from a running session that has seen one datagram.
pub fn snapshot(now: SystemTime) -> RadarSnapshot {
    RadarSnapshot {
        revision: 1,
        session: 1,
        status: SessionStatus::Running { since: now },
        objects: BTreeMap::new(),
        recent: Vec::new(),
        notices: Vec::new(),
        frames_seen: 1,
        last_frame: Some(now),
    }
}

pub fn object(
    ip: Ipv4Addr,
    class_code: u16,
    class_name: &'static str,
    properties: BTreeMap<u8, PropertySnapshot>,
    last_seen: SystemTime,
) -> ObjectSnapshot {
    ObjectSnapshot {
        key: key(ip, class_code),
        source: SocketAddr::from((ip, 3610)),
        class_code,
        class_name,
        last_seen,
        properties,
    }
}
