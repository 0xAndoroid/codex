use super::*;
use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::render::highlight;
use crate::streaming::controller::StreamController;
use crate::terminal_hyperlinks::visible_lines;
use pretty_assertions::assert_eq;
use ratatui::style::Stylize;

#[test]
fn restyle_stops_at_the_first_cell_whose_text_changed() {
    let row = |line: ratatui::text::Line<'static>| HyperlinkLine::new(line);
    let mut emitted = EmittedRows::default();
    let cells = [
        emitted.push(vec![row("fn a".red().into())]),
        emitted.push(vec![row("fn b".red().into())]),
    ];

    emitted.restyle(&[
        row("fn a".blue().into()),
        row("fn b, rewrapped".blue().into()),
    ]);

    assert_eq!(
        cells.map(|cell| visible_lines(cell.read().unwrap().clone())),
        [vec!["fn a".blue().into()], vec!["fn b".red().into()]]
    );
}

#[test]
fn restyle_reaches_emitted_queued_and_live_stream_rows() {
    let cwd = std::env::temp_dir();
    let source = "```rust\nfn palette() -> u32 { 1 }\nfn probe() -> u32 { 2 }\n```\n\nstill";
    let theme = |name| {
        highlight::set_syntax_theme(
            highlight::resolve_theme_by_name(name, /*codex_home*/ None).expect("bundled theme"),
        );
    };
    let stream = || {
        let mut controller = StreamController::new(Some(40), &cwd, HistoryRenderMode::Rich);
        controller.push(source);
        let (emitted, _) = controller.on_commit_tick_batch(/*max_lines*/ 2);
        (controller, emitted.expect("emitted rows"))
    };
    let displayed = |(mut controller, emitted): (StreamController, Box<dyn HistoryCell>)| {
        let (queued, _) = controller.on_commit_tick_batch(usize::MAX);
        let cells = std::iter::once(emitted).chain(queued);
        let lines = cells.flat_map(|cell| cell.display_lines(/*width*/ 42));
        (
            lines.collect::<Vec<_>>(),
            visible_lines(controller.current_tail_lines()),
        )
    };
    let palette = crate::terminal_probe::DefaultColors {
        fg: (76, 79, 105),
        bg: (239, 241, 245),
    };

    let (streamed_dark, restyled, light) =
        crate::terminal_palette::with_test_default_colors(palette, || {
            theme("catppuccin-mocha");
            let (mut controller, emitted) = stream();
            let streamed_dark = emitted.display_lines(/*width*/ 42);
            theme("catppuccin-latte");
            controller.restyle();
            let restyled = displayed((controller, emitted));
            (streamed_dark, restyled, displayed(stream()))
        });

    assert_ne!(streamed_dark, light.0[..streamed_dark.len()]);
    assert_eq!(restyled, light);
}
