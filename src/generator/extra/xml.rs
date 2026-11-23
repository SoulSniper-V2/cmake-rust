//! `cmXMLWriter`/`cmXMLSafe` port for the extra generators.

/// `cmXMLSafe` escaping: XML control characters are escaped, CR is
/// dropped, quotes are escaped in attributes (`quotes = true`) but not in
/// content, and invalid control characters become `[NON-XML-CHAR-0x..]`.
fn escape(text: &str, quotes: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let code = ch as u32;
        let valid = (0x20..=0xD7FF).contains(&code)
            || (0xE000..=0xFFFD).contains(&code)
            || (0x10000..=0x10FFFF).contains(&code)
            || matches!(ch, '\t' | '\n' | '\r');
        if valid {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' if quotes => out.push_str("&quot;"),
                '\'' if quotes => out.push_str("&apos;"),
                '\r' => {}
                _ => out.push(ch),
            }
        } else {
            out.push_str(&format!("[NON-XML-CHAR-0x{code:X}]"));
        }
    }
    out
}

/// `cmXMLWriter` with `\t` indentation.
pub struct XmlWriter {
    pub out: String,
    elements: Vec<String>,
    indent: usize,
    element_open: bool,
    is_content: bool,
}

impl XmlWriter {
    pub fn new() -> XmlWriter {
        XmlWriter {
            out: String::new(),
            elements: Vec::new(),
            indent: 0,
            element_open: false,
            is_content: false,
        }
    }

    pub fn start_document(&mut self, encoding: &str) {
        self.out
            .push_str(&format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?>"));
    }

    pub fn end_document(&mut self) {
        self.out.push('\n');
    }

    fn line_break(&mut self, condition: bool) {
        if condition {
            self.out.push('\n');
            for _ in 0..self.indent {
                self.out.push('\t');
            }
        }
    }

    fn close_start(&mut self) {
        if self.element_open {
            self.out.push('>');
            self.element_open = false;
        }
    }

    pub fn start_element(&mut self, name: &str) {
        self.close_start();
        self.line_break(!self.is_content);
        self.out.push('<');
        self.out.push_str(name);
        self.elements.push(name.to_owned());
        self.indent += 1;
        self.element_open = true;
    }

    pub fn end_element(&mut self) {
        self.indent -= 1;
        if self.element_open {
            self.out.push_str("/>");
        } else {
            self.line_break(!self.is_content);
            self.is_content = false;
            let name = self.elements.last().cloned().unwrap_or_default();
            self.out.push_str("</");
            self.out.push_str(&name);
            self.out.push('>');
        }
        self.elements.pop();
        self.element_open = false;
    }

    pub fn attribute(&mut self, name: &str, value: &str) {
        self.out.push(' ');
        self.out.push_str(name);
        self.out.push_str("=\"");
        self.out.push_str(&escape(value, true));
        self.out.push('"');
    }

    pub fn attribute_i(&mut self, name: &str, value: i64) {
        self.out.push(' ');
        self.out.push_str(name);
        self.out.push_str("=\"");
        self.out.push_str(&value.to_string());
        self.out.push('"');
    }

    /// `<name/>` (cmXMLWriter::Element with no content).
    pub fn element(&mut self, name: &str) {
        self.close_start();
        self.line_break(!self.is_content);
        self.out.push('<');
        self.out.push_str(name);
        self.out.push_str("/>");
    }

    /// `<name>value</name>` (cmXMLWriter::Element(name, value)).
    pub fn element_value(&mut self, name: &str, value: &str) {
        self.start_element(name);
        self.content(value);
        self.end_element();
    }

    pub fn content(&mut self, text: &str) {
        self.close_start();
        self.is_content = true;
        self.out.push_str(&escape(text, false));
    }

    pub fn processing_instruction(&mut self, target: &str, data: &str) {
        self.close_start();
        self.line_break(!self.is_content);
        self.out.push_str("<?");
        self.out.push_str(target);
        self.out.push(' ');
        self.out.push_str(data);
        self.out.push_str("?>");
    }
}
