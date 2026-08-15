//! Repair-document XML → compact German markdown renderer.
//!
//! ISTA's repair families are a different schema from FKB: a
//! `<REPAIRMANUALDOCUMENT>` is a title plus an ordered list of `<OPERATINGSTEP>`s,
//! each carrying paragraphs, hints, lists and figure references. `render_rep`
//! turns one into markdown; the build pipeline (`crate::build`) renders every
//! extracted repair body through it.
//!
//! Vocabulary taken from a 400-document sample of the shipped `REP` set, in
//! frequency order: `PARAGRAPH`, `GRAPHIC`, `ILLUSTRATION`, `OPERATINGSTEP`,
//! `LISTENTRY`, `HOTSPOT`, `HINT`, `TITLE`, `ENTRY`, `REFERENCE`, `LIST`,
//! `EMPHASIZE`, `PROCESSDESC`, `ROW`, `COLSPEC`, `EMPHASIZE2`.
//!
//! **Figures are referenced, never inlined.** A `<GRAPHIC SRC="…">` names an image
//! file that lives outside these databases, so the name is emitted as a marker
//! rather than dropped — a step that says "see figure" is useless without knowing
//! one exists.

use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use thiserror::Error;

/// A failure rendering a repair-document body.
#[derive(Debug, Error)]
pub enum RepError {
    /// The XML could not be parsed.
    #[error("parsing repair-document XML: {0}")]
    Xml(#[from] quick_xml::Error),
}

/// Elements whose text is a standalone block in the output.
fn is_block(tag: &str) -> bool {
    matches!(tag, "PARAGRAPH" | "LISTENTRY" | "ENTRY" | "REFERENCE")
}

/// Render a repair document to compact German markdown.
///
/// `# title` from `PROCESSDESC`, then one `## Schritt N` per `OPERATINGSTEP` with
/// its paragraphs; `LISTENTRY`/`ENTRY` become bullets, a `HINT` becomes a
/// blockquote led by its `TITLE`, and each `GRAPHIC` contributes a
/// `[Abbildung: file]` marker. Inline emphasis is flattened to its text — these
/// are instructions, and the words carry the meaning.
///
/// # Errors
///
/// Returns [`RepError::Xml`] if the body is not well-formed XML.
pub fn render_rep(xml: &str) -> Result<String, RepError> {
    // As in the FKB renderer: no `trim_text`, because an entity reference splits a
    // paragraph into separate text fragments and per-fragment trimming would eat
    // the spaces around the split. Each block is trimmed once when it closes.
    let mut reader = Reader::from_str(xml);
    let mut out: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut stack: Vec<String> = Vec::new();
    let mut step = 0usize;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if is_block(&tag) || tag == "PROCESSDESC" || tag == "TITLE" {
                    text.clear();
                }
                if tag == "OPERATINGSTEP" {
                    step += 1;
                    out.push(format!("## Schritt {step}"));
                }
                stack.push(tag);
            }
            Event::Empty(e) => {
                // A GRAPHIC is empty-element form; keep its file name as a marker.
                if e.name().as_ref() == b"GRAPHIC"
                    && let Some(src) = e.attributes().flatten().find(|a| a.key.as_ref() == b"SRC")
                {
                    let name = String::from_utf8_lossy(&src.value).to_string();
                    out.push(format!("[Abbildung: {name}]"));
                }
            }
            Event::Text(e) => {
                text.push_str(&e.decode().map_err(quick_xml::Error::from)?);
            }
            Event::GeneralRef(e) => {
                if let Some(ch) = e.resolve_char_ref()? {
                    text.push(ch);
                } else {
                    let name = String::from_utf8_lossy(e.as_ref()).to_string();
                    if let Some(resolved) = resolve_predefined_entity(&name) {
                        text.push_str(resolved);
                    }
                }
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let body = text.split_whitespace().collect::<Vec<_>>().join(" ");
                let in_hint = stack.iter().any(|t| t == "HINT");
                match tag.as_str() {
                    "PROCESSDESC" if !body.is_empty() => out.insert(0, format!("# {body}")),
                    "TITLE" if in_hint && !body.is_empty() => out.push(format!("> **{body}**")),
                    "LISTENTRY" | "ENTRY" if !body.is_empty() => out.push(format!("- {body}")),
                    t if is_block(t) && !body.is_empty() => {
                        // A paragraph inside a HINT stays inside the blockquote.
                        out.push(if in_hint {
                            format!("> {body}")
                        } else {
                            body.clone()
                        });
                    }
                    _ => {}
                }
                if is_block(&tag) || tag == "PROCESSDESC" || tag == "TITLE" {
                    text.clear();
                }
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::render_rep;

    /// The real shape, from the shipped `Nockenwelle ausbauen (M40)` document:
    /// a title, numbered steps, a figure marker, a hint blockquote, and the torque
    /// figures that make the document worth having.
    #[test]
    fn renders_a_repair_procedure_with_steps_hints_and_figures() {
        let xml = r#"<REPAIRMANUALDOCUMENT CHARSET="UTF-8" LANGUAGE="de-DE">
  <PROCESSTITLE><PROCESSDESC>Nockenwelle ausbauen (M40)</PROCESSDESC></PROCESSTITLE>
  <PROCESS>
    <OPERATINGSTEP>
      <ILLUSTRATION><GRAPHIC SRC="GRRA4011-066.png" LINKID="G1"/></ILLUSTRATION>
      <PARAGRAPH>Die Schrauben (1 ... 2) l&#246;sen und &#214;lleitung abnehmen.</PARAGRAPH>
      <HINT>
        <TITLE>Einbauhinweis:</TITLE>
        <PARAGRAPH>Dichtung erneuern.</PARAGRAPH>
      </HINT>
      <PARAGRAPH>Schraube (1) = 5,5 Nm</PARAGRAPH>
    </OPERATINGSTEP>
    <OPERATINGSTEP>
      <PARAGRAPH>Nockenwelle abnehmen.</PARAGRAPH>
      <LIST><LISTENTRY>Lagerbock pr&#252;fen</LISTENTRY></LIST>
    </OPERATINGSTEP>
  </PROCESS>
</REPAIRMANUALDOCUMENT>"#;
        let md = render_rep(xml).unwrap();

        assert!(md.starts_with("# Nockenwelle ausbauen (M40)"), "{md}");
        assert!(
            md.contains("## Schritt 1") && md.contains("## Schritt 2"),
            "{md}"
        );
        // Entity references survive with their surrounding spaces intact.
        assert!(
            md.contains("Die Schrauben (1 ... 2) lösen und Ölleitung abnehmen."),
            "{md}"
        );
        // The hint is a blockquote led by its title.
        assert!(
            md.contains("> **Einbauhinweis:**") && md.contains("> Dichtung erneuern."),
            "{md}"
        );
        // The torque spec — the reason a mechanic opens this at all.
        assert!(md.contains("Schraube (1) = 5,5 Nm"), "{md}");
        // The figure is referenced, since the image itself is not in these DBs.
        assert!(md.contains("[Abbildung: GRRA4011-066.png]"), "{md}");
        assert!(md.contains("- Lagerbock prüfen"), "{md}");
    }

    /// A body with no renderable text yields an empty string, so the build step
    /// can skip it rather than storing a blank document.
    #[test]
    fn an_empty_document_renders_to_nothing() {
        let xml =
            "<REPAIRMANUALDOCUMENT><PROCESS><OPERATINGSTEP/></PROCESS></REPAIRMANUALDOCUMENT>";
        // The step heading is still emitted; a document with no steps is empty.
        assert_eq!(render_rep("<REPAIRMANUALDOCUMENT/>").unwrap(), "");
        assert!(!render_rep(xml).unwrap().contains("Schritt 1"));
    }

    /// Malformed XML is an error, never a silently truncated procedure — a
    /// half-rendered repair instruction is worse than none.
    #[test]
    fn malformed_xml_is_an_error() {
        assert!(render_rep("<REPAIRMANUALDOCUMENT><PARAGRAPH>x</WRONG>").is_err());
    }
}
