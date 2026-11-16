//! Session-scoped model selection. Handles are invalid after reconnect or pane replacement.
use super::*;
use tokio::sync::oneshot;

#[derive(Debug, Deserialize)]
pub(crate) struct Model {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub enabled: bool,
    pub status: String,
    #[serde(default)]
    pub variants: Vec<Variant>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Variant {
    pub id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub model: ModelRef,
    pub name: String,
    pub current: bool,
}

pub(crate) fn choices(models: Vec<Model>, current: Option<&ModelRef>) -> Vec<Choice> {
    let mut choices = Vec::new();
    for model in models.into_iter().filter(|model| {
        model.enabled
            && model.kind.as_deref().is_none_or(|kind| kind == "chat")
            && model.status != "deprecated"
    }) {
        for variant in std::iter::once(None).chain(
            model
                .variants
                .into_iter()
                .filter(|v| v.id != "default")
                .map(|v| Some(v.id)),
        ) {
            let reference = ModelRef {
                provider: model.provider.clone(),
                model: model.id.clone(),
                variant,
            };
            choices.push(Choice {
                current: current.is_some_and(|current| current.same_selection(&reference)),
                model: reference,
                name: model.name.clone(),
            });
        }
    }
    choices.sort_by_cached_key(|choice| (!choice.current, choice.model.label()));
    choices
}

#[derive(Clone)]
pub(crate) struct ModelHandle {
    state: Arc<Mutex<State>>,
    requests: mpsc::Sender<Request>,
}

impl Connection {
    pub fn model_handle(&self) -> Result<ModelHandle> {
        anyhow::ensure!(
            self.state.lock().connected,
            "OpenCode is not connected; reconnect first"
        );
        Ok(ModelHandle {
            state: self.state.clone(),
            requests: self.requests.clone(),
        })
    }

    pub fn matches_model_handle(&self, handle: &ModelHandle) -> bool {
        Arc::ptr_eq(&self.state, &handle.state) && self.requests.same_channel(&handle.requests)
    }
}

impl ModelHandle {
    pub fn current(&self) -> Option<ModelRef> {
        self.state
            .lock()
            .session
            .as_ref()
            .and_then(|session| session.model.clone())
    }

    pub fn catalog(&self) -> Result<oneshot::Receiver<Result<Vec<Model>>>> {
        let (reply, rx) = oneshot::channel();
        self.requests
            .try_send(Request::Models(reply))
            .map_err(|_| {
                anyhow::anyhow!("OpenCode connection changed or request queue is unavailable")
            })?;
        Ok(rx)
    }

    pub fn switch(&self, model: ModelRef) -> Result<oneshot::Receiver<Result<()>>> {
        anyhow::ensure!(
            self.state.lock().connected,
            "OpenCode is disconnected; reconnect first"
        );
        let (reply, rx) = oneshot::channel();
        self.requests
            .try_send(Request::SwitchModel { model, reply })
            .map_err(|_| {
                anyhow::anyhow!("OpenCode connection changed or request queue is unavailable")
            })?;
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn choices_filter_non_chat_disabled_and_deprecated_models_and_mark_exact_variant() {
        let catalog = json!([
            {"id":"chat","providerID":"test","name":"Chat","enabled":true,"status":"active","variants":[{"id":"none"},{"id":"high"}]},
            {"id":"image","providerID":"test","name":"Image","type":"image","enabled":true,"status":"active"},
            {"id":"decision","providerID":"test","name":"Decision","type":"decision","enabled":true,"status":"active"},
            {"id":"disabled","providerID":"test","name":"Disabled","enabled":false,"status":"active"},
            {"id":"old","providerID":"test","name":"Old","enabled":true,"status":"deprecated"}
        ]);
        let selected: ModelRef =
            serde_json::from_value(json!({"providerID":"test","id":"chat","variant":"none"}))
                .unwrap();
        let rows = choices(serde_json::from_value(catalog).unwrap(), Some(&selected));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].model, selected);
        assert_eq!(rows[0].model.label(), "test/chat#none");
        assert_eq!(rows.iter().filter(|row| row.current).count(), 1);
        let default = rows.iter().find(|row| row.model.variant.is_none()).unwrap();
        assert!(!default.current);
        assert_eq!(
            serde_json::to_value(&default.model).unwrap(),
            json!({"providerID":"test","id":"chat"})
        );
    }

    #[test]
    fn server_default_variant_matches_the_unqualified_picker_row_not_none() {
        let catalog = json!([
            {"id":"chat","providerID":"test","name":"Chat","enabled":true,"status":"active",
             "variants":[{"id":"default"},{"id":"none"}]}
        ]);
        let selected: ModelRef =
            serde_json::from_value(json!({"providerID":"test","id":"chat","variant":"default"}))
                .unwrap();
        let rows = choices(serde_json::from_value(catalog).unwrap(), Some(&selected));
        assert_eq!(rows.len(), 2);
        assert!(rows[0].current);
        assert!(rows[0].model.variant.is_none());
        assert_eq!(selected.label(), "test/chat");
        assert!(!rows[1].current);
    }
}
