//! FKB (Fehlerkennblatt) XML → compact German markdown renderer.
//!
//! `render_fkb` turns an ISTA FKB body into compact German markdown; the build
//! pipeline (`crate::build`) renders every extracted FKB body through it.

use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
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
/// order. Only elements that can carry paragraph text are listed; a pure
/// grouping element such as UEBERWACHUNGSBEDINGUNG is omitted. FEHLERBESCHREIBUNG
/// is listed because it may hold direct paragraphs, and renders only when it does.
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
    ("CCMELDUNG", "Check-Control-Meldung"),
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
    // Deliberately no `trim_text`: an entity ref splits a paragraph into
    // separate text fragments, and per-fragment trimming would eat the spaces
    // around the split (`U &lt; 9V` -> `U<9V`). Each assembled paragraph is
    // trimmed once at its closing tag instead.
    let mut reader = Reader::from_str(xml);
    let mut blocks: Vec<String> = Vec::new(); // rendered "## H\n\npara\n\npara"
    // Stack of (heading, paragraphs) for the currently-open known sections.
    let mut open: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut in_paragraph = false;
    let mut para = String::new();
    // List nesting depth: paragraphs inside a LIST render as `- ` bullets.
    let mut list_depth: usize = 0;
    // A KLEMME terminal being assembled: (name, status), each captured from its
    // KLEMMENNAME / KLEMMENSTATUS child text.
    let mut klemme: Option<(String, String)> = None;
    // Which KLEMME field the current text feeds: Some(true) = name,
    // Some(false) = status, None = neither.
    let mut klemme_field: Option<bool> = None;

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                match tag.as_str() {
                    "PARAGRAPH" => {
                        in_paragraph = true;
                        para.clear();
                    }
                    "LIST" => list_depth += 1,
                    "KLEMME" => klemme = Some((String::new(), String::new())),
                    "KLEMMENNAME" => klemme_field = Some(true),
                    "KLEMMENSTATUS" => klemme_field = Some(false),
                    _ => {
                        if let Some(h) = heading_for(&tag) {
                            open.push((h, Vec::new()));
                        }
                    }
                }
            }
            Event::Empty(_) => { /* e.g. <PARAGRAPH/> — no text, ignored */ }
            // quick-xml 0.41: the reader no longer inline-unescapes text;
            // `BytesText::decode` yields the (UTF-8) content, and its
            // `EncodingError` is folded into `FkbError::Xml`. Text feeds the open
            // paragraph, or — outside a paragraph — the KLEMME field in progress.
            Event::Text(t) => {
                if in_paragraph {
                    para.push_str(&t.decode().map_err(quick_xml::Error::from)?);
                } else if let (Some(field), Some((name, status))) = (klemme_field, klemme.as_mut())
                {
                    let dst = if field { name } else { status };
                    dst.push_str(&t.decode().map_err(quick_xml::Error::from)?);
                }
            }
            // 0.41 emits entity references as standalone events. Resolve numeric
            // char refs (`&#NN;`/`&#xHH;`) and the five predefined XML entities
            // (lt/gt/amp/apos/quot) inline; any other (DTD-defined) entity has
            // no resolver here and is skipped — FKB bodies carry no DTD.
            Event::GeneralRef(r) if in_paragraph => {
                if let Some(ch) = r.resolve_char_ref()? {
                    para.push(ch);
                } else if let Some(text) =
                    resolve_predefined_entity(&r.decode().map_err(quick_xml::Error::from)?)
                {
                    para.push_str(text);
                }
            }
            Event::End(e) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                match tag.as_str() {
                    "PARAGRAPH" => {
                        in_paragraph = false;
                        let text = para.trim();
                        if !text.is_empty()
                            && let Some((_, paras)) = open.last_mut()
                        {
                            // Inside a LIST every paragraph becomes one bullet.
                            let rendered = if list_depth > 0 {
                                format!("- {text}")
                            } else {
                                text.to_string()
                            };
                            paras.push(rendered);
                        }
                    }
                    "LIST" => list_depth = list_depth.saturating_sub(1),
                    "KLEMMENNAME" | "KLEMMENSTATUS" => klemme_field = None,
                    "KLEMME" => {
                        if let Some((name, status)) = klemme.take() {
                            let (name, status) = (name.trim(), status.trim());
                            if !name.is_empty() {
                                let line = if status.is_empty() {
                                    format!("**Klemme:** {name}")
                                } else {
                                    format!("**Klemme:** {name} — {status}")
                                };
                                // A KLEMME is a standalone block, not section prose.
                                blocks.push(line);
                            }
                        }
                    }
                    _ => {
                        if heading_for(&tag).is_some()
                            && let Some((h, paras)) = open.pop()
                            && !paras.is_empty()
                        {
                            // A fully bulleted section joins with single
                            // newlines; prose paragraphs stay blank-line apart.
                            let bulleted = paras.iter().all(|p| p.starts_with("- "));
                            let sep = if bulleted { "\n" } else { "\n\n" };
                            blocks.push(format!("## {h}\n\n{}", paras.join(sep)));
                        }
                    }
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

    #[test]
    fn resolves_entities_and_char_refs_in_paragraph_text() {
        // quick-xml 0.41 emits entity refs as standalone events between text
        // fragments. Named predefined entities (&lt; &gt; &amp;) and numeric
        // char refs (&#181; = µ) must resolve inline, with the single spaces
        // around each split preserved (not eaten by per-fragment trimming).
        let xml = r#"<FKB LANGUAGE="de-DE">
          <SETZBEDINGUNG>
            <PARAGRAPH>Spannung U &lt; 9V &amp; stabil</PARAGRAPH>
            <PARAGRAPH>Strom &gt; 5 &#181;A</PARAGRAPH>
          </SETZBEDINGUNG>
        </FKB>"#;
        let md = render_fkb(xml).unwrap();
        assert_eq!(
            md,
            "## Setzbedingung\n\nSpannung U < 9V & stabil\n\nStrom > 5 µA"
        );
    }

    #[test]
    fn renders_klemme_and_ccmeldung() {
        let xml = r#"<FKB LANGUAGE="de-DE">
          <UEBERWACHUNGSBEDINGUNG>
            <KLEMME><KLEMMENNAME>Klemme 15</KLEMMENNAME><KLEMMENSTATUS>an</KLEMMENSTATUS></KLEMME>
          </UEBERWACHUNGSBEDINGUNG>
          <CCMELDUNG><PARAGRAPH>Beispielhinweis im Display.</PARAGRAPH></CCMELDUNG>
        </FKB>"#;
        let md = render_fkb(xml).unwrap();
        assert_eq!(
            md,
            "**Klemme:** Klemme 15 — an\n\n\
             ## Check-Control-Meldung\n\nBeispielhinweis im Display."
        );
    }

    #[test]
    fn flattens_lists_to_bullets() {
        let xml = r#"<FKB LANGUAGE="de-DE">
          <MASSNAHMEIMSERVICE>
            <LIST><LISTELEMENT><PARAGRAPH>Erster Schritt.</PARAGRAPH></LISTELEMENT>
            <LISTELEMENT><PARAGRAPH>Zweiter Schritt.</PARAGRAPH></LISTELEMENT></LIST>
          </MASSNAHMEIMSERVICE>
        </FKB>"#;
        let md = render_fkb(xml).unwrap();
        assert_eq!(
            md,
            "## Maßnahme im Service\n\n- Erster Schritt.\n- Zweiter Schritt."
        );
    }
}
