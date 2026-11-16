//! Modal, pane-local inspections. Enter never approves a permission or submits a form.
use super::*;
use crate::{
    compositor::{Component, Context, Event, EventResult},
    ctrl, key,
    opencode::interactions::{
        FileChange, Form, FormField, InteractionHandle, PermissionDecision, PermissionRequest,
    },
    ui::{prompt::PromptAnchor, Prompt},
};
use anyhow::Context as _;
use serde_json::{Map, Value};

#[derive(Clone)]
enum Screen {
    Requests,
    Permission(PermissionRequest),
    Form(Form),
    Options(Form, usize),
    Changes,
    Patch(FileChange),
}

pub(super) struct Interaction {
    handle: InteractionHandle,
    panel: Arc<Mutex<Rect>>,
    input: super::super::prompt::SharedPromptAnchor,
    screen: Screen,
    cursor: usize,
    scroll: usize,
    answers: Arc<Mutex<Map<String, Value>>>,
    error: Arc<Mutex<Option<String>>>,
    follow_selection: bool,
}

impl Interaction {
    pub fn requests(handle: InteractionHandle, panel: Arc<Mutex<Rect>>) -> Self {
        Self {
            handle,
            panel,
            input: Arc::new(Mutex::new(PromptAnchor::default())),
            screen: Screen::Requests,
            cursor: 0,
            scroll: 0,
            answers: Arc::new(Mutex::new(Map::new())),
            error: Arc::new(Mutex::new(None)),
            follow_selection: true,
        }
    }

    pub fn review(handle: InteractionHandle, panel: Arc<Mutex<Rect>>) -> Self {
        Self {
            screen: Screen::Changes,
            ..Self::requests(handle, panel)
        }
    }

    fn back(&mut self) -> EventResult {
        match self.screen {
            Screen::Requests | Screen::Changes => return close(),
            Screen::Patch(_) => self.screen = Screen::Changes,
            Screen::Options(ref form, _) => self.screen = Screen::Form(form.clone()),
            _ => self.screen = Screen::Requests,
        }
        self.cursor = 0;
        self.scroll = 0;
        self.follow_selection = true;
        EventResult::Consumed(None)
    }

    fn visible_fields(&self, form: &Form) -> Vec<usize> {
        let answers = form.normalize(&self.answers.lock());
        form.fields
            .iter()
            .enumerate()
            .filter_map(|(i, field)| (!field.hidden && field.active(&answers)).then_some(i))
            .collect()
    }

    fn edit(&mut self, form: &Form) -> EventResult {
        let fields = self.visible_fields(form);
        let Some(&index) = fields.get(self.cursor) else {
            return EventResult::Consumed(None);
        };
        let field = &form.fields[index];
        if field.kind == "external" {
            *self.error.lock() = Some(
                "Complete the displayed external step, then press Space to acknowledge it.".into(),
            );
        } else if field.kind == "boolean" {
            let mut answers = self.answers.lock();
            let value = answers
                .get(&field.key)
                .or(field.default.as_ref())
                .and_then(Value::as_bool)
                .unwrap_or(false);
            answers.insert(field.key.clone(), Value::Bool(!value));
            *self.error.lock() = None;
        } else if !field.options.is_empty() {
            self.screen = Screen::Options(form.clone(), index);
            self.cursor = 0;
            self.scroll = 0;
        } else {
            return self.edit_text(
                field.clone(),
                form.normalize(&self.answers.lock())
                    .get(&field.key)
                    .cloned(),
            );
        }
        EventResult::Consumed(None)
    }

    fn edit_text(&self, field: FormField, value: Option<Value>) -> EventResult {
        let initial = value
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default();
        let answers = self.answers.clone();
        let error = self.error.clone();
        let anchor = self.input.clone();
        EventResult::Consumed(Some(Box::new(move |compositor, cx| {
            let prompt = Prompt::new("".into(), None, |_, _| Vec::new(), |_, _, _| {})
                .with_line(initial, cx.editor)
                .with_anchor(anchor)
                .with_validate_callback(move |_, text| {
                    match field.parse(text) {
                        Ok(value) => {
                            answers.lock().insert(field.key.clone(), value);
                            *error.lock() = None;
                        }
                        Err(problem) => *error.lock() = Some(problem.to_string()),
                    }
                    None
                });
            compositor.push(Box::new(prompt));
        })))
    }

    fn rows(&self, state: &State, width: usize, theme: &Theme) -> (Vec<ChatRow>, &'static str) {
        let text = theme.get("ui.text");
        let muted = theme.get("ui.text.inactive");
        let mut rows = Vec::new();
        let mut add = |label: &str, style| append(&mut rows, label, width, style);
        let hints = match &self.screen {
            Screen::Requests => {
                add(
                    "Pending approvals and questions",
                    text.add_modifier(Modifier::BOLD),
                );
                add(
                    "Enter inspects only. Nothing is approved automatically.",
                    muted,
                );
                add("", muted);
                let items = request_items(state);
                for (index, (_, label)) in items.iter().enumerate() {
                    add(
                        &format!("{} {label}", if index == self.cursor { "›" } else { " " }),
                        if index == self.cursor {
                            theme.get("ui.text.focus")
                        } else {
                            text
                        },
                    );
                }
                if items.is_empty() {
                    add("No pending requests.", muted);
                }
                "j/k select   Enter inspect   r refresh   Esc close"
            }
            Screen::Permission(permission) => {
                add(
                    &format!("Permission: {}", permission.action),
                    theme.get("warning"),
                );
                add(
                    "Allow once authorizes only this request; it saves no rule.",
                    muted,
                );
                add(&format!("Session: {}", permission.session_id), muted);
                if let Some(session) = &state.session {
                    add(
                        &format!("Directory: {}", session.location.directory.display()),
                        muted,
                    );
                }
                if let Some(message) = &permission.message {
                    add(message, text);
                }
                add("", muted);
                add("Requested resources:", text.add_modifier(Modifier::BOLD));
                for resource in &permission.resources {
                    add(resource, text);
                }
                if permission.resources.is_empty() {
                    add("(No resource list supplied)", theme.get("warning"));
                }
                if let Some(metadata) = &permission.metadata {
                    let mut remainder = metadata.clone();
                    if let Some(files) =
                        metadata
                            .get("files")
                            .and_then(Value::as_array)
                            .filter(|files| {
                                files.iter().all(|file| {
                                    file.get("file").is_some_and(Value::is_string)
                                        && file.get("patch").is_some_and(Value::is_string)
                                })
                            })
                    {
                        for file in files {
                            if let Some(name) = file.get("file").and_then(Value::as_str) {
                                add(name, text.add_modifier(Modifier::BOLD));
                            }
                            if let Some(patch) = file.get("patch").and_then(Value::as_str) {
                                for line in patch.lines() {
                                    add(
                                        line,
                                        if line.starts_with('+') {
                                            theme.get("diff.plus")
                                        } else if line.starts_with('-') {
                                            theme.get("diff.minus")
                                        } else {
                                            text
                                        },
                                    );
                                }
                            }
                            let mut details = file.clone();
                            if let Some(details) = details.as_object_mut() {
                                details.remove("file");
                                details.remove("patch");
                            }
                            if details
                                .as_object()
                                .is_some_and(|details| !details.is_empty())
                            {
                                add(&details.to_string(), muted);
                            }
                        }
                        if let Some(object) = remainder.as_object_mut() {
                            object.remove("files");
                        }
                    }
                    if remainder
                        .as_object()
                        .is_none_or(|object| !object.is_empty())
                    {
                        add(
                            &serde_json::to_string_pretty(&remainder).unwrap_or_default(),
                            text,
                        );
                    }
                }
                if let Some(source) = &permission.source {
                    add(&format!("Source: {source}"), muted);
                }
                if state.permissions.get(&permission.id) != Some(permission) {
                    add(
                        "This request was settled or changed. Return to the list.",
                        theme.get("warning"),
                    );
                }
                "o allow once   d reject   j/k scroll   Esc back"
            }
            Screen::Form(form) => {
                add(&form.title, text.add_modifier(Modifier::BOLD));
                add("Enter edits. Only s submits the complete answer.", muted);
                let answers = form.normalize(&self.answers.lock());
                for (position, index) in self.visible_fields(form).into_iter().enumerate() {
                    let field = &form.fields[index];
                    let value = answers.get(&field.key).map_or("—".into(), |v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string())
                    });
                    add(
                        &format!(
                            "{} {}{}: {value}",
                            if position == self.cursor { "›" } else { " " },
                            field.title.as_deref().unwrap_or(&field.key),
                            if field.required { " *" } else { "" }
                        ),
                        if position == self.cursor {
                            theme.get("ui.text.focus")
                        } else {
                            text
                        },
                    );
                    if let Some(description) = &field.description {
                        add(description, muted);
                    }
                    if let Some(url) = &field.url {
                        add(url, theme.get("ui.text.info"));
                        add(
                            "Complete this step externally; Space acknowledges completion.",
                            theme.get("warning"),
                        );
                    }
                    if !field.options.is_empty() {
                        for option in &field.options {
                            add(&format!("  {} = {}", option.value, option.label), muted);
                        }
                    }
                    if !field.constraints.is_empty() {
                        add(
                            &format!("Constraints: {}", Value::Object(field.constraints.clone())),
                            muted,
                        );
                    }
                    add("", muted);
                }
                if state.forms.get(&form.id) != Some(form) {
                    add(
                        "This question was settled or changed. Return to the list.",
                        theme.get("warning"),
                    );
                }
                "j/k select   Enter edit   s submit   d cancel form"
            }
            Screen::Options(form, index) => {
                let field = &form.fields[*index];
                add(
                    field.title.as_deref().unwrap_or(&field.key),
                    text.add_modifier(Modifier::BOLD),
                );
                let answers = form.normalize(&self.answers.lock());
                let selected = answers.get(&field.key);
                for (position, option) in field.options.iter().enumerate() {
                    let checked = selected.is_some_and(|v| {
                        v.as_str() == Some(&option.value)
                            || v.as_array()
                                .is_some_and(|v| v.contains(&Value::String(option.value.clone())))
                    });
                    add(
                        &format!(
                            "{} [{}] {}",
                            if position == self.cursor { "›" } else { " " },
                            if checked { "x" } else { " " },
                            option.label
                        ),
                        if position == self.cursor {
                            theme.get("ui.text.focus")
                        } else {
                            text
                        },
                    );
                    if let Some(description) = &option.description {
                        add(description, muted);
                    }
                }
                if field.custom {
                    add(
                        "e edits a custom value (multiselect uses a JSON string array).",
                        muted,
                    );
                }
                "j/k select   Space toggle   Enter choose/done   Esc back"
            }
            Screen::Changes => {
                add("Latest turn changes", text.add_modifier(Modifier::BOLD));
                add(
                    "Read-only server snapshot. Opening a file never reloads dirty buffers.",
                    muted,
                );
                if state.review_loading {
                    add("Loading changes…", muted);
                }
                if let Some(error) = &state.review_error {
                    add(error, theme.get("error"));
                }
                if let Some(files) = &state.review {
                    for (index, file) in files.iter().enumerate() {
                        add(
                            &format!(
                                "{} {}  +{} -{}  {}",
                                if index == self.cursor { "›" } else { " " },
                                file.file,
                                file.additions,
                                file.deletions,
                                file.status
                            ),
                            if index == self.cursor {
                                theme.get("ui.text.focus")
                            } else {
                                text
                            },
                        );
                    }
                    if files.is_empty() {
                        add("No recorded changes for the latest turn.", muted);
                    }
                }
                "j/k select   Enter diff   r refresh   Esc close"
            }
            Screen::Patch(file) => {
                add(
                    &format!("{}  +{} -{}", file.file, file.additions, file.deletions),
                    text.add_modifier(Modifier::BOLD),
                );
                for line in file.patch.lines() {
                    add(
                        line,
                        if line.starts_with('+') {
                            theme.get("diff.plus")
                        } else if line.starts_with('-') {
                            theme.get("diff.minus")
                        } else if line.starts_with("@@") {
                            theme.get("ui.text.info")
                        } else {
                            text
                        },
                    );
                }
                "j/k scroll   o open file   Esc back"
            }
        };
        (rows, hints)
    }
}

fn request_items(state: &State) -> Vec<(String, String)> {
    state
        .permissions
        .values()
        .map(|p| (p.id.clone(), format!("Permission: {}", p.action)))
        .chain(
            state
                .forms
                .values()
                .map(|f| (f.id.clone(), format!("Question: {}", f.title))),
        )
        .collect()
}

fn close() -> EventResult {
    EventResult::Consumed(Some(Box::new(|compositor, _| {
        compositor.pop();
    })))
}

fn review_path(root: &std::path::Path, file: &str) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    let relative = std::path::Path::new(file);
    anyhow::ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "Refusing a path outside the session directory"
    );
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    anyhow::ensure!(
        path.starts_with(&root),
        "Refusing a path outside the session directory"
    );
    Ok(path)
}

impl Interaction {
    fn handle_key(&mut self, key: helix_view::input::KeyEvent) -> EventResult {
        let key = &key;
        if matches!(*key, key!(Esc) | key!('q') | ctrl!('c')) {
            return self.back();
        }
        let screen = self.screen.clone();
        let state = self.handle.state.lock();
        let count = match &screen {
            Screen::Requests => request_items(&state).len(),
            Screen::Form(form) => self.visible_fields(form).len(),
            Screen::Options(form, index) => form.fields[*index].options.len(),
            Screen::Changes => state.review.as_ref().map_or(0, Vec::len),
            _ => 0,
        };
        drop(state);
        match *key {
            key!('j') | key!(Down) => {
                if count > 0 {
                    self.cursor = (self.cursor + 1).min(count - 1);
                    self.follow_selection = true;
                } else {
                    self.scroll += 1;
                }
            }
            key!('k') | key!(Up) => {
                if count > 0 {
                    self.cursor = self.cursor.saturating_sub(1);
                    self.follow_selection = true;
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            key!(PageDown) | ctrl!('f') => {
                self.scroll = self.scroll.saturating_add(10);
                self.follow_selection = false;
            }
            key!(PageUp) | ctrl!('b') => {
                self.scroll = self.scroll.saturating_sub(10);
                self.follow_selection = false;
            }
            key!('r') => {
                let result = if matches!(screen, Screen::Changes | Screen::Patch(_)) {
                    self.screen = Screen::Changes;
                    self.cursor = 0;
                    self.scroll = 0;
                    self.handle.review()
                } else {
                    self.handle.refresh()
                };
                if let Err(error) = result {
                    *self.error.lock() = Some(error.to_string());
                }
            }
            key!(Enter) => {
                self.follow_selection = true;
                match screen {
                    Screen::Requests => {
                        let state = self.handle.state.lock();
                        if let Some((id, _)) = request_items(&state).get(self.cursor) {
                            if let Some(permission) = state.permissions.get(id) {
                                self.screen = Screen::Permission(permission.clone());
                            } else if let Some(form) = state.forms.get(id) {
                                self.screen = Screen::Form(form.clone());
                                *self.answers.lock() = form.normalize(&Map::new());
                            }
                            self.cursor = 0;
                            self.scroll = 0;
                            *self.error.lock() = None;
                        }
                    }
                    Screen::Form(form) => return self.edit(&form),
                    Screen::Options(form, index) => {
                        let field = &form.fields[index];
                        if field.kind == "string" {
                            if let Some(option) = field.options.get(self.cursor) {
                                self.answers
                                    .lock()
                                    .insert(field.key.clone(), Value::String(option.value.clone()));
                            }
                        }
                        self.screen = Screen::Form(form);
                        self.cursor = 0;
                        self.scroll = 0;
                    }
                    Screen::Changes => {
                        if let Some(file) = self
                            .handle
                            .state
                            .lock()
                            .review
                            .as_ref()
                            .and_then(|files| files.get(self.cursor))
                        {
                            self.screen = Screen::Patch(file.clone());
                            self.scroll = 0;
                        }
                    }
                    _ => (),
                }
            }
            key!(' ') => match screen {
                Screen::Form(form) => {
                    if let Some(&index) = self.visible_fields(&form).get(self.cursor) {
                        let field = &form.fields[index];
                        if field.kind == "external" {
                            let mut answers = self.answers.lock();
                            if answers.get(&field.key) == Some(&Value::Bool(true)) {
                                answers.remove(&field.key);
                            } else {
                                answers.insert(field.key.clone(), Value::Bool(true));
                            }
                            *self.error.lock() = None;
                        } else {
                            return self.edit(&form);
                        }
                    }
                }
                Screen::Options(form, index) => {
                    let field = &form.fields[index];
                    if let Some(option) = field.options.get(self.cursor) {
                        let mut answers = self.answers.lock();
                        if field.kind == "multiselect" {
                            let values = answers
                                .entry(field.key.clone())
                                .or_insert_with(|| Value::Array(Vec::new()));
                            if let Some(values) = values.as_array_mut() {
                                let value = Value::String(option.value.clone());
                                if let Some(index) = values.iter().position(|v| *v == value) {
                                    values.remove(index);
                                } else {
                                    values.push(value);
                                }
                            }
                        } else {
                            answers.insert(field.key.clone(), Value::String(option.value.clone()));
                        }
                    }
                }
                _ => (),
            },
            key!('e') => {
                if let Screen::Options(form, index) = screen {
                    let field = &form.fields[index];
                    if field.custom {
                        return self.edit_text(
                            field.clone(),
                            self.answers.lock().get(&field.key).cloned(),
                        );
                    }
                }
            }
            key!('o') | key!('d') | key!('s') => {
                let result = match screen {
                    Screen::Permission(permission) if matches!(*key, key!('o') | key!('d')) => {
                        self.handle.permission(
                            permission,
                            if *key == key!('o') {
                                PermissionDecision::Once
                            } else {
                                PermissionDecision::Reject
                            },
                        )
                    }
                    Screen::Form(form) if *key == key!('s') => {
                        self.handle.form(form, Some(self.answers.lock().clone()))
                    }
                    Screen::Form(form) if *key == key!('d') => self.handle.form(form, None),
                    Screen::Patch(file) if *key == key!('o') => {
                        let root = self
                            .handle
                            .state
                            .lock()
                            .session
                            .as_ref()
                            .map(|s| s.location.directory.clone());
                        let path = root
                            .context("Session location unavailable")
                            .and_then(|root| review_path(&root, &file.file));
                        match path {
                            Ok(path) => {
                                return EventResult::Consumed(Some(Box::new(
                                    move |compositor, cx| {
                                        compositor.pop();
                                        match cx
                                            .editor
                                            .open(&path, helix_view::editor::Action::Replace)
                                        {
                                            Ok(_) => {
                                                if let Some(view) =
                                                    compositor.find::<crate::ui::EditorView>()
                                                {
                                                    view.panes.focus_document(cx.editor);
                                                }
                                            }
                                            Err(error) => cx.editor.set_error(error.to_string()),
                                        }
                                    },
                                )))
                            }
                            Err(error) => Err(error),
                        }
                    }
                    _ => Ok(()),
                };
                *self.error.lock() = result.err().map(|e| e.to_string());
            }
            _ => (),
        }
        EventResult::Consumed(None)
    }
}

impl Component for Interaction {
    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => self.handle_key(*key),
            _ => EventResult::Consumed(None),
        }
    }
    fn render(&mut self, viewport: Rect, surface: &mut Buffer, cx: &mut Context) {
        let area = self.panel.lock().intersection(viewport);
        if area.width < 8 || area.height < 6 {
            self.input.lock().area = Rect::default();
            return;
        }
        fill(area, surface, cx.editor.theme.get("ui.popup"));
        let inner = area.clip_left(1).clip_right(1).clip_top(1).clip_bottom(1);
        let body = inner.clip_top(2).clip_bottom(3);
        let state = self.handle.state.lock();
        let count = match &self.screen {
            Screen::Requests => request_items(&state).len(),
            Screen::Form(form) => self.visible_fields(form).len(),
            Screen::Options(form, index) => form.fields[*index].options.len(),
            Screen::Changes => state.review.as_ref().map_or(0, Vec::len),
            _ => 0,
        };
        self.cursor = self.cursor.min(count.saturating_sub(1));
        let (rows, hints) = self.rows(
            &state,
            body.width.saturating_sub(3) as usize,
            &cx.editor.theme,
        );
        if self.follow_selection && body.height > 0 {
            let focus = cx.editor.theme.get("ui.text.focus");
            if let Some(index) = rows
                .iter()
                .position(|row| row.spans.0.iter().any(|span| span.style == focus))
            {
                if index < self.scroll {
                    self.scroll = index;
                }
                if index >= self.scroll + body.height as usize {
                    self.scroll = index + 1 - body.height as usize;
                }
            }
            self.follow_selection = false;
        }
        self.scroll = self
            .scroll
            .min(rows.len().saturating_sub(body.height as usize));
        write(
            inner.with_height(1),
            surface,
            "OpenCode · review / respond",
            cx.editor
                .theme
                .get("ui.text.info")
                .add_modifier(Modifier::BOLD),
        );
        for (index, row) in rows
            .iter()
            .skip(self.scroll)
            .take(body.height as usize)
            .enumerate()
        {
            row.render(
                Rect::new(body.x, body.y + index as u16, body.width, 1),
                surface,
            );
        }
        let status = self
            .error
            .lock()
            .clone()
            .or_else(|| state.interaction_status.clone())
            .unwrap_or_default();
        write(
            Rect::new(inner.x, inner.bottom() - 3, inner.width, 1),
            surface,
            &status,
            cx.editor.theme.get("warning"),
        );
        self.input.lock().area = Rect::new(
            inner.x + 1,
            inner.bottom() - 2,
            inner.width.saturating_sub(2),
            1,
        );
        write(
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            surface,
            hints,
            cx.editor.theme.get("ui.text.inactive"),
        );
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        (Some(Position::default()), CursorKind::Hidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permission() -> PermissionRequest {
        serde_json::from_value(serde_json::json!({
            "id":"per_test", "sessionID":"ses_test", "action":"edit",
            "resources":["/work/project/example.rs"], "metadata":{"command":"write example"}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn inspecting_and_enter_never_approve_a_permission() {
        let connection = Connection::for_test();
        let expected = permission();
        {
            let mut state = connection.state.lock();
            state.connected = true;
            state.permissions.insert(expected.id.clone(), expected);
        }
        let mut ui = Interaction::requests(
            connection.interactions(),
            Arc::new(Mutex::new(Rect::default())),
        );
        ui.handle_key(key!(Enter));
        assert!(matches!(ui.screen, Screen::Permission(_)));
        ui.handle_key(key!(Enter));
        assert!(connection.state.lock().resolving.is_empty());
        ui.handle_key(key!('o'));
        assert!(connection.state.lock().resolving.contains("per_test"));
    }

    #[tokio::test]
    async fn settled_requests_cannot_be_approved_from_an_old_inspection() {
        let connection = Connection::for_test();
        let expected = permission();
        {
            let mut state = connection.state.lock();
            state.connected = true;
            state
                .permissions
                .insert(expected.id.clone(), expected.clone());
        }
        let mut ui = Interaction::requests(
            connection.interactions(),
            Arc::new(Mutex::new(Rect::default())),
        );
        ui.screen = Screen::Permission(expected);
        connection.state.lock().permissions.clear();
        ui.handle_key(key!('o'));
        assert!(connection.state.lock().resolving.is_empty());
        assert!(ui.error.lock().as_ref().unwrap().contains("settled"));
    }

    #[tokio::test]
    async fn forms_only_submit_on_the_explicit_submit_action() {
        let connection = Connection::for_test();
        let form: Form = serde_json::from_value(serde_json::json!({
            "id":"frm_test","sessionID":"ses_test","title":"Confirm",
            "fields":[{"key":"confirm","type":"boolean","required":true}]
        }))
        .unwrap();
        {
            let mut state = connection.state.lock();
            state.connected = true;
            state.forms.insert(form.id.clone(), form.clone());
        }
        let mut ui = Interaction::requests(
            connection.interactions(),
            Arc::new(Mutex::new(Rect::default())),
        );
        ui.screen = Screen::Form(form);
        ui.handle_key(key!(Enter));
        assert_eq!(ui.answers.lock()["confirm"], true);
        assert!(connection.state.lock().resolving.is_empty());
        ui.handle_key(key!('s'));
        assert!(connection.state.lock().resolving.contains("frm_test"));
    }

    #[test]
    fn review_paths_cannot_escape_or_create_files() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("example.rs"), "test")?;
        assert!(review_path(root.path(), "example.rs").is_ok());
        for path in ["../outside", "/etc/passwd", "", "missing.rs"] {
            assert!(review_path(root.path(), path).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.path().join("external"))?;
            assert!(review_path(root.path(), "external").is_err());
        }
        Ok(())
    }
}
