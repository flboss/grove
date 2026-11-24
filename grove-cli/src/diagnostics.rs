use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, Source};
use grove_types::{Diagnostic, LabelStyle, Severity, Span};

pub(crate) fn print(source: &str, name: &str, diagnostics: &[Diagnostic]) {
    if diagnostics.is_empty() {
        return;
    }

    let mut cache = (name, Source::from(source));

    for diagnostic in diagnostics {
        let location = diagnostic
            .labels
            .first()
            .map(|label| label.span.range())
            .unwrap_or(0..0);
        let mut report = Report::build(
            match diagnostic.severity {
                Severity::Error => ReportKind::Error,
                Severity::Warning => ReportKind::Warning,
            },
            (name, location),
        )
        .with_config(Config::default().with_index_type(IndexType::Byte))
        .with_code(diagnostic.code.as_ref())
        .with_message(diagnostic.message.as_ref());

        for label in &diagnostic.labels {
            let color = match label.style {
                LabelStyle::Primary => Color::Red,
                LabelStyle::Secondary => Color::Blue,
            };
            report = report.with_label(
                Label::new((name, label.span.range()))
                    .with_message(label.message.as_ref())
                    .with_color(color),
            );
        }

        for note in &diagnostic.notes {
            report = report.with_note(note.as_ref());
        }

        for help in &diagnostic.help {
            report = report.with_help(help.as_ref());
        }

        if let Err(err) = report.finish().eprint(&mut cache) {
            eprintln!("error: failed to render diagnostic: {err}");
            return;
        }
    }
}

pub(crate) fn abort(message: String, span: Span) -> Diagnostic {
    Diagnostic::error("R0001", message, span, "query aborted here")
}
