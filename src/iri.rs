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

/// Identity kind from Neo4j labels: Class, Property, or Element wins; otherwise
/// exactly one Spike label. Used for IRI minting and same-kind unify.
pub fn identity_kind_from_labels<I, S>(labels: I) -> Option<&'static str>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let labels: Vec<String> = labels
        .into_iter()
        .map(|label| label.as_ref().to_string())
        .collect();
    if labels.iter().any(|label| label == "Class") {
        return Some("Class");
    }
    if labels.iter().any(|label| label == "Property") {
        return Some("Property");
    }
    if labels.iter().any(|label| label == "Element") {
        return Some("Element");
    }
    let spikes: Vec<&'static str> = labels
        .iter()
        .filter_map(|label| match label.as_str() {
            "Signal" => Some("Signal"),
            "Pattern" => Some("Pattern"),
            "Insight" => Some("Insight"),
            "Knowledge" => Some("Knowledge"),
            _ => None,
        })
        .collect();
    if spikes.len() == 1 {
        Some(spikes[0])
    } else {
        None
    }
}

/// Insert spaces before internal ASCII capitals (`graphModel` → `graph Model`).
pub fn split_camel_case(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if index > 0 && ch.is_ascii_uppercase() && chars[index - 1].is_ascii_lowercase() {
            out.push(' ');
        }
        out.push(*ch);
    }
    out
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

#[cfg(test)]
mod tests {
    use super::{identity_kind_from_labels, split_camel_case};

    #[test]
    fn identity_kind_from_labels_prefers_element_over_spike() {
        assert_eq!(
            identity_kind_from_labels(["Knowledge", "Element"]),
            Some("Element")
        );
        assert_eq!(identity_kind_from_labels(["Class"]), Some("Class"));
        assert_eq!(identity_kind_from_labels(["Property"]), Some("Property"));
        assert_eq!(identity_kind_from_labels(["Knowledge"]), Some("Knowledge"));
        assert_eq!(identity_kind_from_labels(["Insight", "Pattern"]), None);
        assert_eq!(identity_kind_from_labels(["Literal"]), None);
    }

    #[test]
    fn split_camel_case_inserts_spaces() {
        assert_eq!(split_camel_case("graphModel"), "graph Model");
        assert_eq!(split_camel_case("rateLimit"), "rate Limit");
        assert_eq!(split_camel_case("Mindreader"), "Mindreader");
    }
}
