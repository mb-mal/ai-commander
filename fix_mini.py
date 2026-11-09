import io

# /compact command in send_current
p = "src/ui/mod.rs"
s = io.open(p, encoding="utf-8").read()
s = s.replace("""    pub fn send_current(&mut self) {
        if self.busy_now() {
            return;
        }
        let text = self.editor.text();
        let atts = std::mem::take(&mut self.attachments);
        if text.trim().is_empty() && atts.is_empty() {
            return;
        }""",
"""    pub fn send_current(&mut self) {
        if self.busy_now() {
            return;
        }
        let text = self.editor.text();
        // К11: /compact — manual context compaction where supported
        if text.trim() == "/compact" {
            self.editor.set_text("");
            let harness = self.cur.harness.clone();
            let sid = self.cur.native_id.clone();
            match self.ensure_backend(&harness) {
                Some(b) => {
                    if b.supports_compact() {
                        let b = b.clone();
                        tokio::spawn(async move {
                            if let Err(e) = b.compact(&sid).await {
                                let _ = e;
                            }
                        });
                        self.toast("compaction requested", false);
                    } else {
                        self.toast("this harness has no manual compaction", true);
                    }
                }
                None => self.toast("harness unavailable", true),
            }
            return;
        }
        let atts = std::mem::take(&mut self.attachments);
        if text.trim().is_empty() && atts.is_empty() {
            return;
        }""")
io.open(p, "w", encoding="utf-8", newline="").write(s)
print("ok /compact")

# mini-map into the status bar
p = "src/ui/widgets.rs"
s = io.open(p, encoding="utf-8").read()
s = s.replace("""fn draw_statusbar(app: &mut App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let lang = app.lang();
    let mut spans = vec![];""",
"""fn draw_statusbar(app: &mut App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let lang = app.lang();
    let mut spans = vec![];

    // К10: the context mini-map — window fill of the CURRENT chat, a mini
    // animation while a compaction runs (`●●●Wr■■■░░`-style)
    {
        let key = (app.cur.harness.clone(), app.cur.native_id.clone());
        let fill = app.ctx_usage.get(&key);
        let running = app.defrag.is_some();
        let mut mini = String::from(" ctx ");
        if running {
            mini.push_str("\u{25cf}\u{25cf}\u{25cf}Wr\u{25a0}\u{25a0}\u{25a0}\u{2591}\u{2591} ");
        } else if let Some((used, size)) = fill {
            if *size > 0 {
                let cells = 10usize;
                let filled = ((*used as f64 / *size as f64) * cells as f64).round() as usize;
                filled.clamp(0, cells).for_each(|_| mini.push('\u{2588}'));
                (cells - filled.clamp(0, cells)).for_each(|_| mini.push('\u{2591}'));
                mini.push_str(&format!(" {}% ", used * 100 / size));
            } else if *used > 0 {
                mini.push_str(&format!(" {}k ", used / 1000));
            }
        }
        spans.push(Span::styled(
            mini,
            Style::new().fg(if running { th.accent } else { th.fg }).bg(th.bg),
        ));
    }
""")
io.open(p, "w", encoding="utf-8", newline="").write(s)
print("ok minimap")
