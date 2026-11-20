//! A small element tree over quick-xml, enough for S3's response documents. Namespaces are
//! ignored (elements are matched by local name).

use quick_xml::Reader;
use quick_xml::events::Event;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Element {
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    /// The first child with this local name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Every child with this local name.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The text of the first child with this name.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

/// Parses a document and returns its root element. The error is a short description for
/// `Error::detail`.
pub(crate) fn parse(doc: &str) -> Result<Element, String> {
    let mut reader = Reader::from_str(doc);
    let mut stack: Vec<Element> = Vec::new();
    loop {
        let event = reader.read_event().map_err(|e| format!("bad XML: {e}"))?;
        match event {
            Event::Start(start) => {
                let name = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                stack.push(Element { name, ..Element::default() });
            }
            Event::Empty(start) => {
                let name = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                let el = Element { name, ..Element::default() };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => return Ok(el),
                }
            }
            Event::End(_) => {
                let Some(el) = stack.pop() else { return Err("bad XML: unbalanced end tag".into()) };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => return Ok(el),
                }
            }
            Event::Text(t) => {
                if let Some(el) = stack.last_mut() {
                    let text = t.xml10_content().map_err(|e| format!("bad XML text: {e}"))?;
                    el.text.push_str(&text);
                }
            }
            Event::CData(c) => {
                if let Some(el) = stack.last_mut() {
                    el.text.push_str(&String::from_utf8_lossy(&c.into_inner()));
                }
            }
            Event::GeneralRef(r) => {
                if let Some(el) = stack.last_mut() {
                    if r.is_char_ref() {
                        let ch = r.resolve_char_ref().map_err(|e| format!("bad XML reference: {e}"))?;
                        el.text.extend(ch);
                    } else {
                        let name = r.decode().map_err(|e| format!("bad XML reference: {e}"))?;
                        let value = quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| format!("bad XML: unknown entity &{name};"))?;
                        el.text.push_str(value);
                    }
                }
            }
            Event::Eof => return Err("bad XML: the document ended early".into()),
            _ => {}
        }
    }
}

/// Escapes text for an XML element body.
pub(crate) fn escape(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

#[cfg(test)]
#[path = "xml_tests.rs"]
mod tests;
