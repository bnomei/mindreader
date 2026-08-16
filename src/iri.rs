//! Deterministic IRI minting and kind/label mapping for graph nodes.
//!
//! Entity and property IRIs use the `mindreader:{kind}/{slug}` shape unless a
//! full IRI is already supplied. Kind and Neo4j label tables stay aligned so
//! MERGE and schema tools can round-trip Class, Property, Element, Spike, and
//! Episode nodes.

/// True when `value` looks like a scheme-qualified IRI (`scheme:rest`).
pub fn is_iri(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    let Some(colon) = value.find(':') else {
        return false;
    };
    if colon == 0 {
        return false;
    }
    value[..colon]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
}

/// Normalize a display name into a path-safe slug, optionally lowercased.
pub fn slugify(name: &str, lower: bool) -> String {
    let mut s = name.trim().to_string();
    if let Some(rest) = s.strip_prefix("mindreader:") {
        if let Some((_, slug)) = rest.split_once('/') {
            s = slug.to_string();
        }
    }
    if lower {
        s = s.to_lowercase();
    }
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "unnamed".into()
    } else {
        out
    }
}

/// Mint `mindreader:{kind}/{slug}` or pass through an already-qualified IRI.
pub fn mint_iri(kind: &str, name_or_slug: &str, lower: bool) -> String {
    if is_iri(name_or_slug) {
        return name_or_slug.to_string();
    }
    format!("mindreader:{kind}/{}", slugify(name_or_slug, lower))
}

/// Extract the kind segment from a `mindreader:` IRI, if present.
pub fn kind_from_iri(iri: &str) -> Option<String> {
    let rest = iri.strip_prefix("mindreader:")?;
    let (kind, _) = rest.split_once('/')?;
    Some(kind.to_string())
}

/// Local name after the last `/`, or the post-scheme remainder as a fallback.
pub fn name_from_iri(iri: &str) -> String {
    if let Some(slash) = iri.rfind('/') {
        if slash + 1 < iri.len() {
            return iri[slash + 1..].to_string();
        }
    }
    if let Some(colon) = iri.find(':') {
        return iri[colon + 1..].to_string();
    }
    iri.to_string()
}

/// Resolve a property local name or IRI to a property IRI.
pub fn property_iri(p: &str) -> String {
    if is_iri(p) {
        p.to_string()
    } else {
        mint_iri("property", p, false)
    }
}

/// Resolve a class local name or IRI to a class IRI.
pub fn class_iri(name_or_iri: &str) -> String {
    if is_iri(name_or_iri) {
        name_or_iri.to_string()
    } else {
        mint_iri("class", name_or_iri, false)
    }
}

/// Whether minting for this kind lowercases the slug by default.
pub fn default_lower_for_kind(kind: &str) -> bool {
    matches!(
        kind,
        "element" | "literal" | "episode" | "signal" | "pattern" | "insight" | "knowledge"
    )
}

/// Map a lowercase kind string to its Neo4j entity label, if known.
pub fn label_for_kind(kind: &str) -> Option<&'static str> {
    Some(match kind.to_ascii_lowercase().as_str() {
        "class" => "Class",
        "property" => "Property",
        "element" => "Element",
        "signal" => "Signal",
        "pattern" => "Pattern",
        "insight" => "Insight",
        "knowledge" => "Knowledge",
        "literal" => "Literal",
        "episode" => "Episode",
        _ => return None,
    })
}

/// Inverse of [`label_for_kind`]: Neo4j label to lowercase kind string.
pub fn kind_for_label(label: &str) -> Option<&'static str> {
    Some(match label {
        "Class" => "class",
        "Property" => "property",
        "Element" => "element",
        "Signal" => "signal",
        "Pattern" => "pattern",
        "Insight" => "insight",
        "Knowledge" => "knowledge",
        "Literal" => "literal",
        "Episode" => "episode",
        _ => return None,
    })
}
