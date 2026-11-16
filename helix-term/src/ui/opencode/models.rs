use super::*;
use crate::{
    compositor,
    job::{Callback, Job},
    opencode::models::{choices, Choice},
    ui::{overlay::overlaid, Picker, PickerColumn},
};
use std::cell::Cell;

fn columns() -> [PickerColumn<Choice, ()>; 2] {
    [
        PickerColumn::new("model", |choice: &Choice, _| {
            format!("{}  ·  {}", choice.model.label(), choice.name).into()
        }),
        PickerColumn::new("selected", |choice: &Choice, _| {
            if choice.current { "current" } else { "" }.into()
        }),
    ]
}

/// Capture the existing pane and connection, never a background document or a
/// newly opened chat. Population starts immediately and cannot reopen a cancelled picker.
pub(crate) fn callback() -> compositor::Callback {
    Box::new(|compositor, cx| {
        let result = (|| -> anyhow::Result<_> {
            let view = compositor
                .find::<crate::ui::EditorView>()
                .ok_or_else(|| anyhow::anyhow!("Editor view is unavailable"))?;
            let id = view
                .panes
                .find(ID)
                .ok_or_else(|| anyhow::anyhow!("Open a chat with :opencode first"))?;
            let handle = view.panes.get_mut::<OpenCode>(id).unwrap().model_handle()?;
            let catalog = handle.catalog()?;
            Ok((id, handle, catalog))
        })();
        let (id, handle, catalog) = match result {
            Ok(value) => value,
            Err(error) => {
                cx.editor.set_error(error.to_string());
                return;
            }
        };
        let current = handle.current();
        let (matcher, injector) = Picker::stream(columns(), ());
        let stream = injector.clone();
        let submitted = Cell::new(false);
        let picker = Picker::with_stream(matcher, 0, injector, |_, _, _| {})
            .with_compositor_callback(move |_, choice, _| {
                // Alt-Enter keeps standard pickers open; submit at most once.
                if submitted.replace(true) {
                    return None;
                }
                let model = choice.model.clone();
                let handle = handle.clone();
                Some(Box::new(move |compositor, cx| {
                    let valid = compositor
                        .find::<crate::ui::EditorView>()
                        .and_then(|view| view.panes.get_mut::<OpenCode>(id))
                        .is_some_and(|pane| pane.matches_model_handle(&handle));
                    if !valid {
                        cx.editor
                            .set_error("OpenCode session changed; reopen :opencode-model");
                        return;
                    }
                    match handle.switch(model.clone()) {
                        Ok(reply) => {
                            cx.jobs.add(Job::with_callback(async move {
                                reply.await.map_err(|_| {
                                    anyhow::anyhow!(
                                        "OpenCode connection changed; model change not confirmed"
                                    )
                                })??;
                                Ok(Callback::Editor(Box::new(move |editor| {
                                    editor.set_status(format!("OpenCode model: {}", model.label()));
                                })))
                            }));
                        }
                        Err(error) => cx.editor.set_error(error.to_string()),
                    }
                }))
            });
        compositor.push(Box::new(overlaid(picker)));
        cx.jobs.add(Job::with_callback(async move {
            let models = catalog.await.map_err(|_| {
                anyhow::anyhow!("OpenCode connection changed while loading models")
            })??;
            let rows = choices(models, current.as_ref());
            anyhow::ensure!(
                !rows.is_empty(),
                "No enabled chat models are available in this session"
            );
            for row in rows {
                if stream.push(row).is_err() {
                    break;
                }
            }
            Ok(Callback::Editor(Box::new(|_| {})))
        }));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo::pattern::{CaseMatching, Normalization};

    fn choice(variant: Option<&str>) -> Choice {
        Choice {
            model: opencode::ModelRef {
                provider: "test".into(),
                model: "chat".into(),
                variant: variant.map(str::to_owned),
            },
            name: "Test chat".into(),
            current: false,
        }
    }

    #[tokio::test]
    async fn streaming_picker_filters_by_variant_and_stops_population_after_drop() {
        let (mut matcher, injector) = Picker::stream(columns(), ());
        injector.push(choice(None)).unwrap();
        injector.push(choice(Some("none"))).unwrap();
        injector.push(choice(Some("high"))).unwrap();
        matcher.pattern.reparse(
            0,
            "test/chat#none",
            CaseMatching::Smart,
            Normalization::Smart,
            false,
        );
        // Nucleo does its matching on background threads.
        for _ in 0..10_000 {
            matcher.tick(10);
            if matcher.snapshot().matched_item_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let snapshot = matcher.snapshot();
        assert_eq!(snapshot.matched_item_count(), 1);
        assert_eq!(
            snapshot
                .matched_items(..)
                .next()
                .unwrap()
                .data
                .model
                .variant
                .as_deref(),
            Some("none")
        );
        let stream = injector.clone();
        let picker = Picker::with_stream(matcher, 0, injector, |_, _, _| {
            panic!("Cancellation must not submit a model")
        });
        drop(picker);
        assert!(stream.push(choice(Some("low"))).is_err());
    }
}
