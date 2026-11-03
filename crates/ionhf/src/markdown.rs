//! A safe subset of Markdown for the Net: headings, emphasis, lists,
//! tables, code, quotes and rules. Links show as their text and address,
//! images as their alt text, HTML as it is written; nothing is fetched or
//! clickable. Parsed once into [`Block`]s, drawn with plain labels.

use eframe::egui::{self, Color32, FontId, Stroke, text::LayoutJob};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub strong: bool,
    pub em: bool,
    pub code: bool,
    pub strike: bool,
    /// A link's address or an image's alt text.
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Span>),
    Para(Vec<Span>),
    /// A list item's paragraph: its depth (0 outermost) and marker ("•",
    /// "3."; empty for a later paragraph of the same item).
    Item(u8, String, Vec<Span>),
    Quote(Vec<Span>),
    Code(String),
    Table(Vec<Vec<Vec<Span>>>),
    Rule,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    strong: u32,
    em: u32,
    strike: u32,
    image: u32,
    quote: u32,
    heading: Option<u8>,
    /// Open lists: the next number (`None`: bullets).
    lists: Vec<Option<u64>>,
    /// The open item's marker, until its first paragraph takes it.
    marker: Option<String>,
    in_item: u32,
    link: Vec<String>,
    code: Option<String>,
    table: Option<Vec<Vec<Vec<Span>>>>,
    row: Vec<Vec<Span>>,
}

impl Builder {
    fn text(&mut self, text: &str, code: bool) {
        if let Some(c) = self.code.as_mut() {
            c.push_str(text);
            return;
        }
        self.spans.push(Span {
            text: text.to_string(),
            strong: self.strong > 0 || self.heading.is_some(),
            em: self.em > 0,
            code,
            strike: self.strike > 0,
            muted: self.image > 0,
        });
    }

    /// The inline text so far as a block of the kind the context says.
    fn flush(&mut self) {
        if self.spans.iter().all(|s| s.text.trim().is_empty()) {
            self.spans.clear();
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let b = if let Some(l) = self.heading {
            Block::Heading(l, spans)
        } else if self.in_item > 0 {
            let depth = self.lists.len().saturating_sub(1).min(8) as u8;
            Block::Item(depth, self.marker.take().unwrap_or_default(), spans)
        } else if self.quote > 0 {
            Block::Quote(spans)
        } else {
            Block::Para(spans)
        };
        self.blocks.push(b);
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Start(t) => self.start(t),
            Event::End(t) => self.end(t),
            Event::Text(t) => self.text(&t, false),
            Event::Code(t) => self.text(&t, true),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, true),
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t, false),
            Event::SoftBreak => self.text(" ", false),
            Event::HardBreak => self.text("\n", false),
            Event::Rule => {
                self.flush();
                self.blocks.push(Block::Rule);
            }
            Event::FootnoteReference(t) => self.text(&format!("[{t}]"), false),
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }, false),
        }
    }

    fn start(&mut self, t: Tag) {
        match t {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    _ => 4,
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                self.in_item += 1;
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}.", *n - 1)
                    }
                    _ => "•".to_string(),
                };
                self.marker = Some(marker);
            }
            Tag::Emphasis => self.em += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.link.push(dest_url.to_string()),
            Tag::Image { .. } => self.image += 1,
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Vec::new());
            }
            Tag::TableHead | Tag::TableRow => self.row.clear(),
            Tag::TableCell => self.spans.clear(),
            _ => {}
        }
    }

    fn end(&mut self, t: TagEnd) {
        match t {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(c) = self.code.take() {
                    self.blocks.push(Block::Code(c.trim_end_matches('\n').to_string()));
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.in_item = self.in_item.saturating_sub(1);
                self.marker = None;
            }
            TagEnd::Emphasis => self.em = self.em.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                let url = self.link.pop().unwrap_or_default();
                let shown: String = self.spans.iter().map(|s| s.text.as_str()).collect();
                if !url.is_empty() && !shown.ends_with(url.as_str()) {
                    self.spans.push(Span {
                        text: format!(" ({url})"),
                        muted: true,
                        ..Span::default()
                    });
                }
            }
            TagEnd::Image => self.image = self.image.saturating_sub(1),
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                self.row.push(cell);
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let row = std::mem::take(&mut self.row);
                if let Some(t) = self.table.as_mut() {
                    t.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.blocks.push(Block::Table(t));
                }
            }
            _ => {}
        }
    }
}

pub fn parse(md: &str) -> Vec<Block> {
    let mut b = Builder::default();
    for e in Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        b.event(e);
    }
    b.flush();
    b.blocks
}

/// Markdown parsed once and kept until its text changes.
#[derive(Default)]
pub struct Cached {
    text: String,
    blocks: Vec<Block>,
}

impl Cached {
    pub fn get(&mut self, md: &str) -> &[Block] {
        if md != self.text || (self.blocks.is_empty() && !md.trim().is_empty()) {
            self.text = md.to_string();
            self.blocks = parse(md);
        }
        &self.blocks
    }
}

const BODY: f32 = 14.0;

fn job(spans: &[Span], size: f32, color: Color32) -> LayoutJob {
    let mut j = LayoutJob::default();
    for s in spans {
        let mut f = egui::TextFormat {
            font_id: if s.code {
                FontId::monospace(size - 1.0)
            } else {
                FontId::proportional(size)
            },
            color: if s.muted {
                theme::MUTED
            } else if s.strong {
                Color32::WHITE
            } else {
                color
            },
            italics: s.em,
            ..Default::default()
        };
        if s.code {
            f.background = theme::RAISED;
        }
        if s.strike {
            f.strikethrough = Stroke::new(1.0, color);
        }
        j.append(&s.text, 0.0, f);
    }
    j
}

fn para(ui: &mut egui::Ui, j: LayoutJob) {
    ui.add(egui::Label::new(j).wrap().selectable(true));
}

/// Draws `blocks` with labels only.
pub fn show(ui: &mut egui::Ui, blocks: &[Block]) {
    for (i, b) in blocks.iter().enumerate() {
        match b {
            Block::Heading(l, spans) => {
                if i > 0 {
                    ui.add_space(6.0);
                }
                let size = match l {
                    1 => 21.0,
                    2 => 18.0,
                    3 => 16.0,
                    _ => 15.0,
                };
                para(ui, job(spans, size, theme::TEXT));
            }
            Block::Para(spans) => para(ui, job(spans, BODY, theme::TEXT)),
            Block::Item(depth, marker, spans) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(4.0 + *depth as f32 * 18.0);
                    let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    ui.painter().text(
                        r.right_top() + egui::vec2(-4.0, 1.0),
                        egui::Align2::RIGHT_TOP,
                        marker,
                        FontId::proportional(BODY),
                        theme::MUTED,
                    );
                    para(ui, job(spans, BODY, theme::TEXT));
                });
            }
            Block::Quote(spans) => {
                egui::Frame::new()
                    .stroke(Stroke::new(0.0, theme::LINE))
                    .inner_margin(egui::Margin {
                        left: 10,
                        ..Default::default()
                    })
                    .show(ui, |ui| {
                        let top = ui.cursor().min;
                        para(ui, job(spans, BODY, theme::MUTED));
                        let bottom = ui.min_rect().max.y;
                        ui.painter()
                            .vline(top.x - 8.0, top.y..=bottom, Stroke::new(2.0, theme::LINE));
                    });
            }
            Block::Code(text) => {
                egui::Frame::new()
                    .fill(theme::INSET)
                    .corner_radius(4)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).monospace().color(theme::TEXT)).selectable(true),
                        );
                    });
            }
            Block::Table(rows) => {
                let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
                egui::Grid::new(("md_table", i))
                    .num_columns(cols)
                    .striped(true)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for (r, row) in rows.iter().enumerate() {
                            for cell in row {
                                let mut spans = cell.clone();
                                if r == 0 {
                                    spans.iter_mut().for_each(|s| s.strong = true);
                                }
                                para(ui, job(&spans, BODY, theme::TEXT));
                            }
                            ui.end_row();
                        }
                    });
            }
            Block::Rule => {
                ui.separator();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn all_text(blocks: &[Block]) -> String {
        let mut out = String::new();
        for b in blocks {
            match b {
                Block::Heading(_, s) | Block::Para(s) | Block::Item(_, _, s) | Block::Quote(s) => out += &text(s),
                Block::Code(c) => out += c,
                Block::Table(rows) => rows.iter().flatten().for_each(|c| out += &text(c)),
                Block::Rule => {}
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn headings_emphasis_and_code() {
        let b = parse("# Net\n\nSome **bold**, *em*, ~~old~~ and `code`.\n\n---\n\n```\nfreq 3.975\n```");
        assert_eq!(
            b[0],
            Block::Heading(
                1,
                vec![Span {
                    text: "Net".into(),
                    strong: true,
                    ..Span::default()
                }]
            )
        );
        let Block::Para(p) = &b[1] else { panic!("{b:?}") };
        assert!(p.iter().any(|s| s.text == "bold" && s.strong));
        assert!(p.iter().any(|s| s.text == "em" && s.em));
        assert!(p.iter().any(|s| s.text == "old" && s.strike));
        assert!(p.iter().any(|s| s.text == "code" && s.code));
        assert_eq!(b[2], Block::Rule);
        assert_eq!(b[3], Block::Code("freq 3.975".into()));
    }

    #[test]
    fn lists_nest_and_number() {
        let b = parse("1. one\n2. two\n   - inner\n3. three\n\n- a\n- b\n");
        let items: Vec<(u8, String, String)> = b
            .iter()
            .filter_map(|b| match b {
                Block::Item(d, m, s) => Some((*d, m.clone(), text(s))),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            [
                (0, "1.".into(), "one".into()),
                (0, "2.".into(), "two".into()),
                (1, "•".into(), "inner".into()),
                (0, "3.".into(), "three".into()),
                (0, "•".into(), "a".into()),
                (0, "•".into(), "b".into()),
            ]
        );
        let b = parse("4. four\n5. five\n");
        assert!(matches!(&b[0], Block::Item(0, m, _) if m == "4."));
    }

    #[test]
    fn tables_keep_their_cells() {
        let b = parse("| Item | Who |\n|---|---|\n| Weather | KJ5ET |\n| Drill | **AG5B** |\n");
        let Block::Table(rows) = &b[0] else { panic!("{b:?}") };
        assert_eq!(rows.len(), 3);
        assert_eq!(text(&rows[0][1]), "Who");
        assert_eq!(text(&rows[2][1]), "AG5B");
        assert!(rows[2][1][0].strong);
    }

    #[test]
    fn links_and_images_are_only_text() {
        let b = parse(
            "See [the plan](https://example.com/plan) and <https://ares.example>.\n\n\
             ![map of the county](https://example.com/map.png)\n\n<b>raw</b> <script>x</script>",
        );
        let t = all_text(&b);
        assert!(
            t.contains("See the plan (https://example.com/plan) and https://ares.example."),
            "{t}"
        );
        assert!(
            !t.contains("https://ares.example (https"),
            "an autolink shows once: {t}"
        );
        assert!(t.contains("map of the county"), "{t}");
        assert!(!t.contains("map.png"), "an image's address is not even shown: {t}");
        assert!(
            t.contains("<b>raw</b>") && t.contains("<script>x</script>"),
            "HTML as written: {t}"
        );
        // The spans have no link in them to follow: an address is a muted span.
        let Block::Para(p) = &b[0] else { panic!() };
        assert!(p.iter().any(|s| s.muted && s.text == " (https://example.com/plan)"));
    }

    /// The roles of the widgets one pass of `f` made.
    fn roles(f: impl FnMut(&mut egui::Ui)) -> Vec<egui::Role> {
        let ctx = egui::Context::default();
        // Widget infos are kept only for this debug view.
        ctx.global_style_mut(|s| s.debug.show_interactive_widgets = true);
        let mut f = f;
        let mut out = Vec::new();
        for _ in 0..2 {
            let mut o = ctx.run_ui(egui::RawInput::default(), &mut f);
            o.textures_delta.clear();
            assert!(o.platform_output.commands.is_empty());
            out = ctx.viewport(|v| {
                let w = &v.this_pass.widgets;
                w.layers()
                    .flat_map(|(_, rects)| rects.iter())
                    .filter_map(|r| w.info(r.id).map(|i| i.role))
                    .collect()
            });
        }
        out
    }

    #[test]
    fn drawing_makes_nothing_clickable() {
        let blocks = parse(
            "# A

[link](https://example.com) <https://x.example>

- x
  1. y

             | a | [b](https://b.example) |
|---|---|
| 1 | 2 |

> [quoted](https://q.example)

```
c
```",
        );
        let found = roles(|ui| show(ui, &blocks));
        assert!(!found.is_empty(), "the labels are seen");
        assert!(!found.contains(&egui::Role::Link), "{found:?}");
        // The check itself sees a link when there is one.
        let control = roles(|ui| {
            ui.hyperlink("https://example.com");
        });
        assert!(control.contains(&egui::Role::Link), "{control:?}");
    }
}
