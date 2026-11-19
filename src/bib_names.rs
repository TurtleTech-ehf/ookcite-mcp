//! BibTeX name lists: von, last, jr, and first, in that order.
//!
//! A token is a von token when its first letter at brace level 0 is
//! lowercase. `De` and `Le` start with a capital, so they stay in the last
//! name. A name wrapped in one brace group is one organisation and is not
//! split. Braces and LaTeX commands are copied through; nothing here turns
//! `{\o}` or `\'{e}` into a Unicode letter or an ASCII substitute.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BibName {
    pub first: String,
    pub von: String,
    pub last: String,
    pub jr: String,
    pub literal: bool,
}

pub fn parse_list(value: &str) -> Vec<BibName> {
    split_and(value)
        .into_iter()
        .map(|name| parse_name(name.trim()))
        .filter(|name| !name.last.is_empty() || !name.first.is_empty())
        .collect()
}

#[cfg(test)]
pub fn format_list(names: &[BibName]) -> String {
    names
        .iter()
        .map(format_name)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// `von last, jr, first`. Empty parts are left out. A literal is one brace group.
pub fn format_parts(von: &str, last: &str, jr: &str, first: &str) -> String {
    format_name(&BibName {
        first: first.trim().to_string(),
        von: von.trim().to_string(),
        last: last.trim().to_string(),
        jr: jr.trim().to_string(),
        literal: false,
    })
}

pub fn format_name(name: &BibName) -> String {
    if name.literal {
        return format!("{{{}}}", name.last.trim());
    }
    let mut head = String::new();
    if !name.von.trim().is_empty() {
        head.push_str(name.von.trim());
        head.push(' ');
    }
    head.push_str(name.last.trim());
    let jr = name.jr.trim();
    let first = name.first.trim();
    match (jr.is_empty(), first.is_empty()) {
        (true, true) => head,
        (true, false) => format!("{head}, {first}"),
        (false, true) => format!("{head}, {jr}"),
        (false, false) => format!("{head}, {jr}, {first}"),
    }
}

fn parse_name(name: &str) -> BibName {
    let name = name.trim().trim_end_matches(',').trim();
    if let Some(inner) = whole_brace_group(name) {
        return BibName {
            first: String::new(),
            von: String::new(),
            last: inner,
            jr: String::new(),
            literal: true,
        };
    }
    let parts = split_commas(name);
    match parts.len() {
        0 => BibName {
            first: String::new(),
            von: String::new(),
            last: String::new(),
            jr: String::new(),
            literal: false,
        },
        1 => split_first_von_last(&parts[0]),
        2 => {
            let (von, last) = split_von_last(&parts[0]);
            BibName {
                first: parts[1].trim().to_string(),
                von,
                last,
                jr: String::new(),
                literal: false,
            }
        }
        _ => {
            let (von, last) = split_von_last(&parts[0]);
            let jr = parts[1..parts.len() - 1]
                .iter()
                .map(|part| part.trim())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            BibName {
                first: parts[parts.len() - 1].trim().to_string(),
                von,
                last,
                jr,
                literal: false,
            }
        }
    }
}

/// No comma: first, then von, then last. The last token stays in the last name.
fn split_first_von_last(value: &str) -> BibName {
    let tokens = tokens(value);
    if tokens.is_empty() {
        return BibName {
            first: String::new(),
            von: String::new(),
            last: String::new(),
            jr: String::new(),
            literal: false,
        };
    }
    let last_index = tokens.len() - 1;
    let von_at = tokens
        .iter()
        .enumerate()
        .take(last_index)
        .find(|(_, token)| is_von(token))
        .map(|(index, _)| index);
    let Some(von_at) = von_at else {
        return BibName {
            first: String::new(),
            von: String::new(),
            last: tokens.join(" "),
            jr: String::new(),
            literal: false,
        };
    };
    let mut von_end = von_at;
    while von_end < last_index && is_von(&tokens[von_end]) {
        von_end += 1;
    }
    BibName {
        first: tokens[..von_at].join(" "),
        von: tokens[von_at..von_end].join(" "),
        last: tokens[von_end..].join(" "),
        jr: String::new(),
        literal: false,
    }
}

/// The part before a comma is von plus last. Von is only the leading run.
/// The last token stays in the last name even when it is lowercase.
fn split_von_last(value: &str) -> (String, String) {
    let tokens = tokens(value);
    if tokens.is_empty() {
        return (String::new(), String::new());
    }
    let last_index = tokens.len() - 1;
    let mut von_end = 0;
    while von_end < last_index && is_von(&tokens[von_end]) {
        von_end += 1;
    }
    (tokens[..von_end].join(" "), tokens[von_end..].join(" "))
}

fn tokens(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    for ch in value.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth -= 1;
                current.push(ch);
            }
            ch if ch.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn is_von(token: &str) -> bool {
    let mut depth: i32 = 0;
    for ch in token.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ch if depth == 0 && ch.is_alphabetic() => return ch.is_lowercase(),
            _ => {}
        }
    }
    false
}

fn whole_brace_group(value: &str) -> Option<String> {
    let chars: Vec<char> = value.chars().collect();
    if chars.first() != Some(&'{') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut closed = None;
    for (index, ch) in chars.iter().enumerate() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    closed = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let closed = closed?;
    if chars[closed + 1..].iter().any(|ch| !ch.is_whitespace()) {
        return None;
    }
    let inner: String = chars[1..closed].iter().collect();
    let inner = inner.trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

fn split_and(value: &str) -> Vec<String> {
    split_on(value, " and ")
}

fn split_commas(value: &str) -> Vec<String> {
    split_on(value, ",")
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn split_on(value: &str, needle: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth: i32 = 0;
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ if depth == 0 && chars[index..].starts_with(&needle) => {
                parts.push(chars[start..index].iter().collect());
                index += needle.len();
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(chars[start..].iter().collect());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(name: &str) {
        let formatted = format_list(&parse_list(name));
        assert_eq!(formatted, name, "parse/format changed the name");
    }

    #[test]
    fn particles_and_junior_round_trip() {
        round_trip("van der Spoel, David");
        round_trip("de Buyl, Pierre");
        round_trip("in 't Veld, Pieter J.");
        round_trip("Steele, Jr., Guy L.");
        round_trip("van Mourik, Tanja");
        round_trip("de Graaf, Joost");
        round_trip("van der Walt, Stéfan J.");
        round_trip("de Gironcoli, Stefano");
        round_trip("da Silva Santos, Luiz Bonino");
        round_trip("{'t Hoen}, Peter A. C.");
    }

    #[test]
    fn capitalised_de_and_le_stay_in_the_last_name() {
        round_trip("De Souza, Roger A.");
        round_trip("Le Bris, Claude");
        let parsed = parse_list("De Souza, Roger A.");
        assert_eq!(parsed[0].von, "");
        assert_eq!(parsed[0].last, "De Souza");
        assert_eq!(parsed[0].first, "Roger A.");
        assert_ne!(format_list(&parsed), "Souza, Roger A. De");
        let bris = parse_list("Le Bris, Claude");
        assert_eq!(bris[0].last, "Le Bris");
        assert_ne!(format_list(&bris), "Bris, Claude Le");
    }

    #[test]
    fn a_moved_particle_is_emitted_in_bibtex_order() {
        let formatted = format_parts("De", "Souza", "", "Roger A.");
        assert_eq!(formatted, "De Souza, Roger A.");
        assert_ne!(formatted, "Souza, Roger A. De");
        assert_eq!(
            format_parts("van der", "Spoel", "", "David"),
            "van der Spoel, David"
        );
        assert_eq!(
            format_parts("", "Steele", "Jr.", "Guy L."),
            "Steele, Jr., Guy L."
        );
    }

    #[test]
    fn latex_accents_stay_byte_for_byte() {
        round_trip("S{\\o}rensen, M. R.");
        round_trip("B{\\'e}land, Laurent K.");
        round_trip("Pober{\\v{z}}nik, Matic");
        round_trip("H{\\'e}meryck, Anne");
        round_trip("J{\\'o}nsson, Hannes");
        round_trip("Gra{\\v{z}}ulis, Saulius");
        let parsed = parse_list("S{\\o}rensen, M. R.");
        assert_eq!(parsed[0].last, "S{\\o}rensen");
        assert!(!format_list(&parsed).contains("So/rensen"));
        assert!(!format_list(&parsed).contains('ø'));
        let long = parse_list("B{\\'e}land, Laurent Karim");
        assert_eq!(long[0].first, "Laurent Karim");
        assert_eq!(format_name(&long[0]), "B{\\'e}land, Laurent Karim");
    }

    #[test]
    fn a_braced_organisation_is_one_author() {
        round_trip("{JCGM committee}");
        round_trip("{Joint Committee for Guides in Metrology}");
        round_trip("{IEEE Computer Society}");
        round_trip("{LAMMPS developers}");
        let parsed = parse_list("{Joint Committee for Guides in Metrology}");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].literal);
        assert_eq!(parsed[0].last, "Joint Committee for Guides in Metrology");
        let mixed = parse_list("{Zarr Developers} and van der Walt, St{\\'e}fan J.");
        assert_eq!(mixed.len(), 2);
        assert!(mixed[0].literal);
        assert_eq!(mixed[1].von, "van der");
        assert_eq!(mixed[1].last, "Walt");
    }

    #[test]
    fn and_inside_braces_does_not_split_the_name() {
        let parsed = parse_list("{National Institute of Standards and Technology}");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].literal);
    }
}
