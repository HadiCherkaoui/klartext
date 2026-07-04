//! FKB (Fehlerkennblatt) XML → compact German markdown renderer.
//!
//! Exercised by this module's unit tests now; the binary wires `render_fkb`
//! into its build pipeline in a later step, so its items are otherwise unused
//! for the moment.
#![allow(dead_code)]

use quick_xml::Reader;
use quick_xml::events::Event;
use thiserror::Error;

/// A failure rendering an FKB body.
#[derive(Debug, Error)]
pub enum FkbError {
    /// The XML could not be parsed.
    #[error("parsing FKB XML: {0}")]
    Xml(#[from] quick_xml::Error),
}

/// Known FKB content sections → German markdown heading, in a stable render
/// order. Grouping elements (FEHLERBESCHREIBUNG/UEBERWACHUNGSBEDINGUNG) are NOT
/// listed — only the leaf sections that carry paragraph text.
const SECTIONS: &[(&str, &str)] = &[
    ("FEHLERBESCHREIBUNG", "Fehlerbeschreibung"),
    ("BESCHREIBUNG", "Beschreibung"),
    ("SETZBEDINGUNG", "Setzbedingung"),
    ("SPANNUNGSBEDINGUNG", "Spannungsbedingung"),
    ("FAHRZUSTAND", "Fahrzustand"),
    ("ZEITBEDINGUNG", "Zeitbedingung"),
    ("MASSNAHMEIMSERVICE", "Maßnahme im Service"),
    ("FEHLERAUSWIRKUNG", "Fehlerauswirkung"),
    ("SICHTBAREAUSWIRKUNG", "Sichtbare Auswirkung"),
    ("PANNENHINWEIS", "Pannenhinweis"),
    ("FAHRERINFORMATION", "Fahrerinformation"),
    ("WARNLEUCHTE", "Warnleuchte"),
    ("SERVICEHINWEIS", "Servicehinweis"),
    ("FEHLERORTTEXT", "Fehlerort"),
];

fn heading_for(tag: &str) -> Option<&'static str> {
    SECTIONS.iter().find(|(el, _)| *el == tag).map(|(_, h)| *h)
}

/// Render an FKB body to compact German markdown: one `## Heading` per non-empty
/// known section, paragraphs blank-line separated, empty sections dropped.
///
/// # Errors
///
/// Returns [`FkbError::Xml`] if the FKB body is not well-formed XML.
pub fn render_fkb(xml: &str) -> Result<String, FkbError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut blocks: Vec<String> = Vec::new(); // rendered "## H\n\npara\n\npara"
    // Stack of (heading, paragraphs) for the currently-open known sections.
    let mut open: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut in_paragraph = false;
    let mut para = String::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                if tag == "PARAGRAPH" {
                    in_paragraph = true;
                    para.clear();
                } else if let Some(h) = heading_for(&tag) {
                    open.push((h, Vec::new()));
                }
            }
            Event::Empty(_) => { /* e.g. <PARAGRAPH/> — no text, ignored */ }
            // quick-xml 0.41: the reader no longer inline-unescapes text;
            // `BytesText::decode` yields the (UTF-8) content, and its
            // `EncodingError` is folded into `FkbError::Xml`.
            Event::Text(t) if in_paragraph => {
                para.push_str(&t.decode().map_err(quick_xml::Error::from)?);
            }
            Event::End(e) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                if tag == "PARAGRAPH" {
                    in_paragraph = false;
                    let text = para.trim();
                    if !text.is_empty()
                        && let Some((_, paras)) = open.last_mut()
                    {
                        paras.push(text.to_string());
                    }
                } else if heading_for(&tag).is_some()
                    && let Some((h, paras)) = open.pop()
                    && !paras.is_empty()
                {
                    blocks.push(format!("## {h}\n\n{}", paras.join("\n\n")));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(blocks.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic FKB body — invented German-ish text, not ISTA content.
    const SAMPLE: &str = r#"<FKB LANGUAGE="de-DE">
      <FEHLERBESCHREIBUNG>
        <BESCHREIBUNG><PARAGRAPH/></BESCHREIBUNG>
        <SETZBEDINGUNG>
          <PARAGRAPH>Beispiel: Fehler wird bei Kurzschluss erkannt.</PARAGRAPH>
          <PARAGRAPH>Zweiter Absatz.</PARAGRAPH>
        </SETZBEDINGUNG>
      </FEHLERBESCHREIBUNG>
      <ZEITBEDINGUNG><PARAGRAPH>Mindestens 2 s.</PARAGRAPH></ZEITBEDINGUNG>
      <MASSNAHMEIMSERVICE><PARAGRAPH>Steuergeraet pruefen.</PARAGRAPH></MASSNAHMEIMSERVICE>
    </FKB>"#;

    #[test]
    fn renders_known_sections_in_order_dropping_empty() {
        let md = render_fkb(SAMPLE).unwrap();
        // Empty BESCHREIBUNG dropped; three non-empty sections, in document order.
        assert_eq!(
            md,
            "## Setzbedingung\n\nBeispiel: Fehler wird bei Kurzschluss erkannt.\n\nZweiter Absatz.\n\n\
             ## Zeitbedingung\n\nMindestens 2 s.\n\n\
             ## Maßnahme im Service\n\nSteuergeraet pruefen."
        );
    }
}
