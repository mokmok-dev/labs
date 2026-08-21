//! Typed, resolved model of the ECHONET Lite Machine Readable Appendix (MRA).
//!
//! The MRA JSON is intentionally permissive in shape (`oneOf`, `$ref`, per-release
//! overloads), so we load it as a [`serde_json::Value`] tree and flatten it into
//! normalized structures here. All traversal is sorted so generation is
//! deterministic regardless of key order in the source files.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A single EPC entry, one of possibly many for the same EPC across releases.
#[derive(Debug, Clone)]
pub struct PropertyEntry {
    /// EPC byte.
    pub epc: u8,
    /// English short name as given in the appendix (may collide or be a keyword).
    pub raw_short_name: String,
    /// English display name.
    pub name_en: String,
    /// Access rules.
    pub get: Access,
    pub set: Access,
    pub inf: Access,
    /// Resolved data shape.
    pub data: Data,
    /// Description text (en).
    pub description_en: String,
}

/// Access rule for a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Required,
    Optional,
    NotApplicable,
}

/// Normalized description of a property's EDT (data) shape after resolving refs.
#[derive(Debug, Clone)]
pub enum Data {
    /// An enumeration of named byte values (state). `size` is byte width.
    State {
        size: usize,
        /// Sorted list of (edt bytes, canonical identifier, english name).
        variants: Vec<(Vec<u8>, String, String)>,
    },
    /// A numeric value with an optional scale applied for the SI unit.
    Number {
        size: usize,
        signed: bool,
        /// Raw integer min (already multiplied by scale when scale > 1 integer).
        min: i128,
        max: i128,
        /// Explicit valid raw values when the appendix lists them as an `enum`
        /// instead of a range (empty for range-typed numbers).
        valid_values: Vec<i128>,
        /// Scale factor (SI) represented as (numerator, denominator) for 0.1/0.001/10.
        scale: Option<(u32, u32)>,
        unit: Option<String>,
    },
    /// Raw byte string of fixed or variable length.
    Raw { min_size: usize, max_size: usize },
    /// Level: a raw byte, or base byte plus a maximum offset.
    Level { base: u8, maximum: u8 },
    /// BCD-encoded time (hours/minutes/seconds) or date.
    Time { size: usize },
    /// BCD-encoded date.
    Date { size: usize },
    /// A multi-field object (concatenated sub elements).
    Object { fields: Vec<Field> },
    /// An array of repeated items of fixed item size.
    Array { item_size: usize, max_items: usize },
    /// A choice among shapes (collapsed from `oneOf`).
    OneOf(Vec<Data>),
}

/// A sub-field within an [`Data::Object`].
#[derive(Debug, Clone)]
pub struct Field {
    pub data: Data,
}

/// A device class file: the class code and its raw property entries.
#[derive(Debug)]
pub struct ClassFile {
    pub class: u16,
    pub class_name_en: String,
    pub short_name: String,
    pub kind: ClassKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    Device,
    SuperClass,
    NodeProfile,
}

/// A fully loaded and resolved model of the whole appendix.
#[derive(Debug)]
pub struct Model {
    pub metadata_version: Option<String>,
    pub classes: Vec<ClassFile>,
    /// All property entries across every class, keyed by class then EPC.
    pub properties: BTreeMap<u16, Vec<PropertyEntry>>,
}

impl Model {
    /// Load and resolve all appendix JSON from the given vendor root.
    pub fn load(vendor: &Path) -> Result<Self, String> {
        let definitions = load_definitions(&vendor.join("definitions/definitions.json"))?;

        let mut classes = Vec::new();
        let mut properties: BTreeMap<u16, Vec<PropertyEntry>> = BTreeMap::new();

        load_class_dir(
            &vendor.join("nodeProfile"),
            ClassKind::NodeProfile,
            &definitions,
            &mut classes,
            &mut properties,
        )?;
        load_class_dir(
            &vendor.join("superClass"),
            ClassKind::SuperClass,
            &definitions,
            &mut classes,
            &mut properties,
        )?;
        load_class_dir(
            &vendor.join("devices"),
            ClassKind::Device,
            &definitions,
            &mut classes,
            &mut properties,
        )?;

        // Sort classes deterministically by class code.
        classes.sort_by_key(|c| c.class);

        let metadata_version = load_metadata(&vendor.join("metaData.json"));

        Ok(Self {
            metadata_version,
            classes,
            properties,
        })
    }
}

fn load_metadata(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("metaData")?
        .get("dataVersion")?
        .as_str()
        .map(str::to_owned)
}

fn load_definitions(path: &Path) -> Result<BTreeMap<String, Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))?;
    let map = value
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{path:?}: missing \"definitions\""))?;
    // Sort keys for determinism.
    let mut out = BTreeMap::new();
    for (k, v) in map {
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

fn load_class_dir(
    dir: &Path,
    kind: ClassKind,
    definitions: &BTreeMap<String, Value>,
    classes: &mut Vec<ClassFile>,
    properties: &mut BTreeMap<u16, Vec<PropertyEntry>>,
) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("read dir {dir:?}: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|e| format!("read {file:?}: {e}"))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| format!("parse {file:?}: {e}"))?;
        let class = parse_class_code(value.get("eoj"))?;

        // Update the class registry.
        if !classes.iter().any(|c| c.class == class) {
            let class_name_en = value
                .get("className")
                .and_then(|c| c.get("en"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let short_name = value
                .get("shortName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            classes.push(ClassFile {
                class,
                class_name_en,
                short_name,
                kind,
            });
        }

        let entries = properties.entry(class).or_default();
        if let Some(list) = value.get("elProperties").and_then(Value::as_array) {
            for prop in list {
                if let Some(entry) = parse_property(prop, definitions, &file.to_string_lossy())? {
                    entries.push(entry);
                }
            }
        }
    }
    Ok(())
}

fn parse_class_code(v: Option<&Value>) -> Result<u16, String> {
    let s = v
        .and_then(Value::as_str)
        .ok_or_else(|| "missing eoj".to_string())?;
    u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("bad eoj {s:?}: {e}"))
}

fn parse_epc(v: Option<&Value>) -> Result<u8, String> {
    let s = v
        .and_then(Value::as_str)
        .ok_or_else(|| "missing epc".to_string())?;
    u8::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("bad epc {s:?}: {e}"))
}

fn parse_access(v: Option<&Value>) -> Access {
    match v.and_then(Value::as_str) {
        Some("required") => Access::Required,
        Some("notApplicable") => Access::NotApplicable,
        _ => Access::Optional,
    }
}

fn parse_property(
    prop: &Value,
    definitions: &BTreeMap<String, Value>,
    path: &str,
) -> Result<Option<PropertyEntry>, String> {
    let Some(epc) = parse_epc(prop.get("epc")).ok() else {
        return Ok(None);
    };
    let raw_short_name = prop
        .get("shortName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let name_en = prop
        .get("propertyName")
        .and_then(|p| p.get("en"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let description_en = prop
        .get("descriptions")
        .and_then(|p| p.get("en"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let access = prop.get("accessRule");
    let get = parse_access(access.and_then(|a| a.get("get")));
    let set = parse_access(access.and_then(|a| a.get("set")));
    let inf = parse_access(access.and_then(|a| a.get("inf")));

    let Some(data) = prop.get("data") else {
        return Ok(None);
    };
    let data = resolve_data(data, definitions, path)?;

    Ok(Some(PropertyEntry {
        epc,
        raw_short_name,
        name_en,
        get,
        set,
        inf,
        data,
        description_en,
    }))
}

/// Resolve a `data` node, following `$ref` and collapsing `oneOf`.
fn resolve_data(
    node: &Value,
    definitions: &BTreeMap<String, Value>,
    path: &str,
) -> Result<Data, String> {
    match node {
        Value::Object(map) => {
            // $ref
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                let name = reference
                    .strip_prefix("#/definitions/")
                    .ok_or_else(|| format!("{path}: unsupported ref {reference:?}"))?;
                let def = definitions
                    .get(name)
                    .ok_or_else(|| format!("{path}: unknown definition {name:?}"))?;
                return resolve_data(def, definitions, path);
            }
            // oneOf
            if let Some(one_of) = map.get("oneOf").and_then(Value::as_array) {
                let mut variants = Vec::new();
                for v in one_of {
                    variants.push(resolve_data(v, definitions, path)?);
                }
                return Ok(Data::OneOf(variants));
            }
            // type-specific
            let ty = map.get("type").and_then(Value::as_str).unwrap_or("");
            match ty {
                "state" => Ok(resolve_state(map)?),
                "number" => Ok(resolve_number(map)?),
                "raw" => Ok(resolve_raw(map)?),
                "level" => Ok(resolve_level(map)?),
                "time" => Ok(resolve_time(map)?),
                "date" => Ok(resolve_date(map)?),
                "date-time" => Ok(Data::Time {
                    size: parse_size(map).unwrap_or(6),
                }),
                "object" => Ok(resolve_object(map, definitions, path)?),
                "array" => Ok(resolve_array(map)?),
                // `bitmap` is a bit-packed field; treat as a fixed-size opaque
                // byte string for the pass-through codec.
                "bitmap" => Ok(resolve_fixed_raw(map)?),
                // `numericValue` maps EDT bytes to a numeric multiplier; the
                // byte itself is a fixed-size value for the pass-through codec.
                "numericValue" => Ok(resolve_fixed_raw(map)?),
                other => Err(format!("{path}: unsupported data type {other:?}")),
            }
        },
        _ => Err(format!("{path}: data is not an object")),
    }
}

fn parse_size(map: &serde_json::Map<String, Value>) -> Option<usize> {
    map.get("size").and_then(Value::as_u64).map(|v| v as usize)
}

fn resolve_state(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let size = parse_size(map).unwrap_or(1);
    let mut variants = Vec::new();
    if let Some(list) = map.get("enum").and_then(Value::as_array) {
        for v in list {
            let edt = v.get("edt").and_then(Value::as_str).unwrap_or_default();
            // Range-style EDTs (e.g. "0x000a...0x0013") denote many values and
            // cannot be a single named variant; skip them.
            let Some(bytes) = parse_hex_bytes_opt(edt) else {
                continue;
            };
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let desc = v
                .get("descriptions")
                .and_then(|d| d.get("en"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            variants.push((bytes, name, desc));
        }
    }
    // Sort deterministically.
    variants.sort_by(|a, b| a.0.cmp(&b.0));
    variants.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    Ok(Data::State { size, variants })
}

fn resolve_number(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let format = map.get("format").and_then(Value::as_str).unwrap_or("uint8");
    let (size, signed) = match format {
        "uint8" => (1usize, false),
        "uint16" => (2, false),
        "uint32" => (4, false),
        "int8" => (1, true),
        "int16" => (2, true),
        "int32" => (4, true),
        other => return Err(format!("unsupported number format {other:?}")),
    };
    // Explicit value list instead of a range, e.g. number_1-20-21-22-23-24.
    let valid_values: Vec<i128> = if let Some(list) = map.get("enum").and_then(Value::as_array) {
        let mut vals: Vec<i128> = list
            .iter()
            .filter_map(Value::as_i64)
            .map(i128::from)
            .collect();
        vals.sort_unstable();
        vals.dedup();
        vals
    } else {
        Vec::new()
    };
    // Range falls back to the enum extremes when the appendix gives an enum.
    let range_min = map.get("minimum").and_then(Value::as_i64);
    let range_max = map.get("maximum").and_then(Value::as_i64);
    let min = match range_min {
        Some(v) => i128::from(v),
        None => valid_values.first().copied().unwrap_or_default(),
    };
    let max = match range_max {
        Some(v) => i128::from(v),
        None => valid_values.last().copied().unwrap_or_default(),
    };

    // scale: `multiple` is a fractional SI denominator (e.g. 0.1, 0.001),
    // `multipleOf` is an integer step.
    let scale = if let Some(m) = map.get("multiple").and_then(Value::as_f64) {
        Some(scaling(m).ok_or_else(|| format!("unsupported multiple {m:?}"))?)
    } else if let Some(m) = map.get("multipleOf").and_then(Value::as_f64) {
        if m <= 0.0 {
            return Err(format!("unsupported multipleOf {m:?}"));
        }
        Some((m as u32, 1))
    } else {
        None
    };
    let unit = map.get("unit").and_then(Value::as_str).map(str::to_owned);

    Ok(Data::Number {
        size,
        signed,
        min,
        max,
        valid_values,
        scale,
        unit,
    })
}

/// Map a real-world `multiple` to an exact (numerator, denominator) pair.
fn scaling(v: f64) -> Option<(u32, u32)> {
    if (v - 0.1).abs() < 1e-9 {
        Some((1, 10))
    } else if (v - 0.001).abs() < 1e-9 {
        Some((1, 1000))
    } else if (v - 10.0).abs() < 1e-9 {
        Some((10, 1))
    } else {
        None
    }
}

fn resolve_raw(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let min = map
        .get("minSize")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(1);
    let max = map
        .get("maxSize")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(min);
    Ok(Data::Raw {
        min_size: min,
        max_size: max,
    })
}

/// A fixed-size opaque byte string (used for `bitmap`/`numericValue`).
fn resolve_fixed_raw(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let size = parse_size(map).unwrap_or(1);
    Ok(Data::Raw {
        min_size: size,
        max_size: size,
    })
}

fn resolve_level(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let base = map
        .get("base")
        .and_then(Value::as_str)
        .map(parse_hex_bytes)
        .transpose()?
        .and_then(|b| b.first().copied())
        .unwrap_or(0);
    let maximum = map.get("maximum").and_then(Value::as_u64).unwrap_or(0) as u8;
    Ok(Data::Level { base, maximum })
}

fn resolve_time(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let size = parse_size(map).unwrap_or(2);
    Ok(Data::Time { size })
}

fn resolve_date(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let size = parse_size(map).unwrap_or(4);
    Ok(Data::Date { size })
}

fn resolve_object(
    map: &serde_json::Map<String, Value>,
    definitions: &BTreeMap<String, Value>,
    path: &str,
) -> Result<Data, String> {
    let mut fields = Vec::new();
    if let Some(props) = map.get("properties").and_then(Value::as_array) {
        for p in props {
            let element = p
                .get("element")
                .ok_or_else(|| format!("{path}: object field missing element"))?;
            let data = resolve_data(element, definitions, path)?;
            fields.push(Field { data });
        }
    }
    Ok(Data::Object { fields })
}

fn resolve_array(map: &serde_json::Map<String, Value>) -> Result<Data, String> {
    let item_size = map.get("itemSize").and_then(Value::as_u64).unwrap_or(1) as usize;
    let max_items = map.get("maxItems").and_then(Value::as_u64).unwrap_or(0) as usize;
    Ok(Data::Array {
        item_size,
        max_items,
    })
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let body = s.trim_start_matches("0x");
    if !body.len().is_multiple_of(2) {
        return Err(format!("odd-length hex {s:?}"));
    }
    let mut out = Vec::new();
    for i in (0..body.len()).step_by(2) {
        let byte =
            u8::from_str_radix(&body[i..i + 2], 16).map_err(|e| format!("bad hex {s:?}: {e}"))?;
        out.push(byte);
    }
    Ok(out)
}

/// Parse a hex byte string, returning `None` for range-style or invalid values.
fn parse_hex_bytes_opt(s: &str) -> Option<Vec<u8>> {
    if s.contains("...") || s.is_empty() {
        return None;
    }
    parse_hex_bytes(s).ok()
}

/// Deterministic, unique Rust-safe snake_case identifiers.
///
/// Handles: camelCase→snake_case, Rust keywords, leading digits, duplicates.
pub fn sanitize_ident(
    raw: &str,
    used: &mut BTreeSet<String>,
) -> String {
    let mut s = String::new();
    if raw.is_empty() {
        s.push('_');
    }
    // Insert underscores between camelCase transitions.
    let lower: Vec<char> = raw.chars().collect();
    for (i, &c) in lower.iter().enumerate() {
        if c.is_ascii_uppercase()
            && i > 0
            && (lower[i - 1].is_ascii_lowercase() || lower[i - 1].is_ascii_digit())
        {
            s.push('_');
        }
        s.push(c.to_ascii_lowercase());
    }

    // Strip characters invalid in identifiers.
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let mut ident = cleaned;
    // Leading digit.
    if ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        ident.insert(0, '_');
    }
    // Rust keywords.
    if is_keyword(&ident) {
        ident.insert(0, '_');
    }
    // Dedup.
    let mut candidate = ident.clone();
    let mut n = 2;
    while used.contains(&candidate) {
        candidate = format!("{ident}_{n}");
        n += 1;
    }
    used.insert(candidate.clone());
    candidate
}

/// Convert a snake_case identifier into UpperCamelCase for use as an enum variant.
///
/// `operation_status` → `OperationStatus`; underscores are removed and the letter
/// after each underscore is capitalized.
pub fn to_camel_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut capitalize = true;
    for c in s.chars() {
        if c == '_' {
            capitalize = true;
        } else if capitalize {
            out.extend(c.to_uppercase());
            capitalize = false;
        } else {
            out.push(c);
        }
    }
    // Ensure a plausible variant even for pathological inputs.
    if out.is_empty() {
        out.push('_');
    }
    // A leading digit would be invalid in an enum variant; prefix with an
    // underscore unless the input already handled it.
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "try"
    )
}
