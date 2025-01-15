use crate::SlashCommandWorkingSet;
use crate::{slash_command::SlashCommandCompletionProvider, AssistantPanel, InlineAssistant};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use editor::{actions::Tab, CurrentLineHighlight, Editor, EditorElement, EditorEvent, EditorStyle};
use futures::{
    future::{self, BoxFuture, Shared},
    FutureExt,
};
use fuzzy::StringMatchCandidate;
use gpui::{
    actions, point, size, transparent_black, Action, AppContext, BackgroundExecutor, Bounds,
    EventEmitter, Focusable, Global, Model, PromptLevel, ReadGlobal, Subscription, Task, TextStyle,
    TitlebarOptions, UpdateGlobal, WindowBounds, WindowHandle, WindowOptions,
};
use heed::{
    types::{SerdeBincode, SerdeJson, Str},
    Database, RoTxn,
};
use language::{language_settings::SoftWrap, Buffer, LanguageRegistry};
use language_model::{
    LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage, Role,
};
use parking_lot::RwLock;
use picker::{Picker, PickerDelegate};
use release_channel::ReleaseChannel;
use rope::Rope;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{
    cmp::Reverse,
    future::Future,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
use text::LineEnding;
use theme::ThemeSettings;
use ui::{
    div, prelude::*, IconButtonShape, KeyBinding, ListItem, ListItemSpacing, ModelContext,
    ParentElement, Render, SharedString, Styled, Tooltip, Window,
};
use util::{ResultExt, TryFutureExt};
use uuid::Uuid;
use workspace::Workspace;
use zed_actions::InlineAssist;

actions!(
    prompt_library,
    [
        NewPrompt,
        DeletePrompt,
        DuplicatePrompt,
        ToggleDefaultPrompt
    ]
);

/// Init starts loading the PromptStore in the background and assigns
/// a shared future to a global.
pub fn init(cx: &mut AppContext) {
    let db_path = paths::prompts_dir().join("prompts-library-db.0.mdb");
    let prompt_store_future = PromptStore::new(db_path, cx.background_executor().clone())
        .then(|result| future::ready(result.map(Arc::new).map_err(Arc::new)))
        .boxed()
        .shared();
    cx.set_global(GlobalPromptStore(prompt_store_future))
}

const BUILT_IN_TOOLTIP_TEXT: &'static str = concat!(
    "This prompt supports special functionality.\n",
    "It's read-only, but you can remove it from your default prompt."
);

/// This function opens a new prompt library window if one doesn't exist already.
/// If one exists, it brings it to the foreground.
///
/// Note that, when opening a new window, this waits for the PromptStore to be
/// initialized. If it was initialized successfully, it returns a window handle
/// to a prompt library.
pub fn open_prompt_library(
    language_registry: Arc<LanguageRegistry>,
    cx: &mut AppContext,
) -> Task<Result<WindowHandle<PromptLibrary>>> {
    let existing_window = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<PromptLibrary>());
    if let Some(existing_window) = existing_window {
        existing_window
            .update(cx, |_, window, _| window.activate_window())
            .ok();
        Task::ready(Ok(existing_window))
    } else {
        let store = PromptStore::global(cx);
        cx.spawn(|cx| async move {
            let store = store.await?;
            cx.update(|cx| {
                let app_id = ReleaseChannel::global(cx).app_id();
                let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
                cx.open_window(
                    WindowOptions {
                        titlebar: Some(TitlebarOptions {
                            title: Some("Prompt Library".into()),
                            appears_transparent: cfg!(target_os = "macos"),
                            traffic_light_position: Some(point(px(9.0), px(9.0))),
                        }),
                        app_id: Some(app_id.to_owned()),
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |window, cx| {
                        cx.new_model(|cx| PromptLibrary::new(store, language_registry, window, cx))
                    },
                )
            })?
        })
    }
}

pub struct PromptLibrary {
    store: Arc<PromptStore>,
    language_registry: Arc<LanguageRegistry>,
    prompt_editors: HashMap<PromptId, PromptEditor>,
    active_prompt_id: Option<PromptId>,
    picker: Model<Picker<PromptPickerDelegate>>,
    pending_load: Task<()>,
    _subscriptions: Vec<Subscription>,
}

struct PromptEditor {
    title_editor: Model<Editor>,
    body_editor: Model<Editor>,
    token_count: Option<usize>,
    pending_token_count: Task<Option<()>>,
    next_title_and_body_to_save: Option<(String, Rope)>,
    pending_save: Option<Task<Option<()>>>,
    _subscriptions: Vec<Subscription>,
}

struct PromptPickerDelegate {
    store: Arc<PromptStore>,
    selected_index: usize,
    matches: Vec<PromptMetadata>,
}

enum PromptPickerEvent {
    Selected { prompt_id: PromptId },
    Confirmed { prompt_id: PromptId },
    Deleted { prompt_id: PromptId },
    ToggledDefault { prompt_id: PromptId },
}

impl EventEmitter<PromptPickerEvent> for Picker<PromptPickerDelegate> {}

impl PickerDelegate for PromptPickerDelegate {
    type ListItem = ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut AppContext) -> SharedString {
        if self.store.prompt_count() == 0 {
            "No prompts.".into()
        } else {
            "No prompts found matching your search.".into()
        }
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut ModelContext<Picker<Self>>,
    ) {
        self.selected_index = ix;
        if let Some(prompt) = self.matches.get(self.selected_index) {
            cx.emit(PromptPickerEvent::Selected {
                prompt_id: prompt.id,
            });
        }
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut AppContext) -> Arc<str> {
        "Search...".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut ModelContext<Picker<Self>>,
    ) -> Task<()> {
        let search = self.store.search(query);
        let prev_prompt_id = self.matches.get(self.selected_index).map(|mat| mat.id);
        cx.spawn_in(window, |this, mut cx| async move {
            let (matches, selected_index) = cx
                .background_executor()
                .spawn(async move {
                    let matches = search.await;

                    let selected_index = prev_prompt_id
                        .and_then(|prev_prompt_id| {
                            matches.iter().position(|entry| entry.id == prev_prompt_id)
                        })
                        .unwrap_or(0);
                    (matches, selected_index)
                })
                .await;

            this.update_in(&mut cx, |this, window, cx| {
                this.delegate.matches = matches;
                this.delegate.set_selected_index(selected_index, window, cx);
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _: &mut Window, cx: &mut ModelContext<Picker<Self>>) {
        if let Some(prompt) = self.matches.get(self.selected_index) {
            cx.emit(PromptPickerEvent::Confirmed {
                prompt_id: prompt.id,
            });
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut ModelContext<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut ModelContext<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let prompt = self.matches.get(ix)?;
        let default = prompt.default;
        let prompt_id = prompt.id;
        let element = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(h_flex().h_5().line_height(relative(1.)).child(Label::new(
                prompt.title.clone().unwrap_or("Untitled".into()),
            )))
            .end_slot::<IconButton>(default.then(|| {
                IconButton::new("toggle-default-prompt", IconName::SparkleFilled)
                    .toggle_state(true)
                    .icon_color(Color::Accent)
                    .shape(IconButtonShape::Square)
                    .tooltip(Tooltip::text("Remove from Default Prompt"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(PromptPickerEvent::ToggledDefault { prompt_id })
                    }))
            }))
            .end_hover_slot(
                h_flex()
                    .gap_2()
                    .child(if prompt_id.is_built_in() {
                        div()
                            .id("built-in-prompt")
                            .child(Icon::new(IconName::FileLock).color(Color::Muted))
                            .tooltip(move |window, cx| {
                                Tooltip::with_meta(
                                    "Built-in prompt",
                                    None,
                                    BUILT_IN_TOOLTIP_TEXT,
                                    window,
                                    cx,
                                )
                            })
                            .into_any()
                    } else {
                        IconButton::new("delete-prompt", IconName::Trash)
                            .icon_color(Color::Muted)
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text("Delete Prompt"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(PromptPickerEvent::Deleted { prompt_id })
                            }))
                            .into_any_element()
                    })
                    .child(
                        IconButton::new("toggle-default-prompt", IconName::Sparkle)
                            .toggle_state(default)
                            .selected_icon(IconName::SparkleFilled)
                            .icon_color(if default { Color::Accent } else { Color::Muted })
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text(if default {
                                "Remove from Default Prompt"
                            } else {
                                "Add to Default Prompt"
                            }))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(PromptPickerEvent::ToggledDefault { prompt_id })
                            })),
                    ),
            );
        Some(element)
    }

    fn render_editor(
        &self,
        editor: &Model<Editor>,
        _: &mut Window,
        cx: &mut ModelContext<Picker<Self>>,
    ) -> Div {
        h_flex()
            .bg(cx.theme().colors().editor_background)
            .rounded_md()
            .overflow_hidden()
            .flex_none()
            .py_1()
            .px_2()
            .mx_1()
            .child(editor.clone())
    }
}

impl PromptLibrary {
    fn new(
        store: Arc<PromptStore>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        let delegate = PromptPickerDelegate {
            store: store.clone(),
            selected_index: 0,
            matches: Vec::new(),
        };

        let picker = cx.new_model(|cx| {
            let picker = Picker::uniform_list(delegate, window, cx)
                .modal(false)
                .max_height(None);
            picker.focus(window, cx);
            picker
        });
        Self {
            store: store.clone(),
            language_registry,
            prompt_editors: HashMap::default(),
            active_prompt_id: None,
            pending_load: Task::ready(()),
            _subscriptions: vec![cx.subscribe_in(&picker, window, Self::handle_picker_event)],
            picker,
        }
    }

    fn handle_picker_event(
        &mut self,
        _: &Model<Picker<PromptPickerDelegate>>,
        event: &PromptPickerEvent,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        match event {
            PromptPickerEvent::Selected { prompt_id } => {
                self.load_prompt(*prompt_id, false, window, cx);
            }
            PromptPickerEvent::Confirmed { prompt_id } => {
                self.load_prompt(*prompt_id, true, window, cx);
            }
            PromptPickerEvent::ToggledDefault { prompt_id } => {
                self.toggle_default_for_prompt(*prompt_id, window, cx);
            }
            PromptPickerEvent::Deleted { prompt_id } => {
                self.delete_prompt(*prompt_id, window, cx);
            }
        }
    }

    pub fn new_prompt(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        // If we already have an untitled prompt, use that instead
        // of creating a new one.
        if let Some(metadata) = self.store.first() {
            if metadata.title.is_none() {
                self.load_prompt(metadata.id, true, window, cx);
                return;
            }
        }

        let prompt_id = PromptId::new();
        let save = self.store.save(prompt_id, None, false, "".into());
        self.picker
            .update(cx, |picker, cx| picker.refresh(window, cx));
        cx.spawn_in(window, |this, mut cx| async move {
            save.await?;
            this.update_in(&mut cx, |this, window, cx| {
                this.load_prompt(prompt_id, true, window, cx)
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn save_prompt(
        &mut self,
        prompt_id: PromptId,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        const SAVE_THROTTLE: Duration = Duration::from_millis(500);

        if prompt_id.is_built_in() {
            return;
        }

        let prompt_metadata = self.store.metadata(prompt_id).unwrap();
        let prompt_editor = self.prompt_editors.get_mut(&prompt_id).unwrap();
        let title = prompt_editor.title_editor.read(cx).text(cx);
        let body = prompt_editor.body_editor.update(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap()
                .read(cx)
                .as_rope()
                .clone()
        });

        let store = self.store.clone();
        let executor = cx.background_executor().clone();

        prompt_editor.next_title_and_body_to_save = Some((title, body));
        if prompt_editor.pending_save.is_none() {
            prompt_editor.pending_save = Some(cx.spawn_in(window, |this, mut cx| {
                async move {
                    loop {
                        let title_and_body = this.update(&mut cx, |this, _| {
                            this.prompt_editors
                                .get_mut(&prompt_id)?
                                .next_title_and_body_to_save
                                .take()
                        })?;

                        if let Some((title, body)) = title_and_body {
                            let title = if title.trim().is_empty() {
                                None
                            } else {
                                Some(SharedString::from(title))
                            };
                            store
                                .save(prompt_id, title, prompt_metadata.default, body)
                                .await
                                .log_err();
                            this.update_in(&mut cx, |this, window, cx| {
                                this.picker
                                    .update(cx, |picker, cx| picker.refresh(window, cx));
                                cx.notify();
                            })?;

                            executor.timer(SAVE_THROTTLE).await;
                        } else {
                            break;
                        }
                    }

                    this.update(&mut cx, |this, _cx| {
                        if let Some(prompt_editor) = this.prompt_editors.get_mut(&prompt_id) {
                            prompt_editor.pending_save = None;
                        }
                    })
                }
                .log_err()
            }));
        }
    }

    pub fn delete_active_prompt(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        if let Some(active_prompt_id) = self.active_prompt_id {
            self.delete_prompt(active_prompt_id, window, cx);
        }
    }

    pub fn duplicate_active_prompt(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        if let Some(active_prompt_id) = self.active_prompt_id {
            self.duplicate_prompt(active_prompt_id, window, cx);
        }
    }

    pub fn toggle_default_for_active_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(active_prompt_id) = self.active_prompt_id {
            self.toggle_default_for_prompt(active_prompt_id, window, cx);
        }
    }

    pub fn toggle_default_for_prompt(
        &mut self,
        prompt_id: PromptId,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(prompt_metadata) = self.store.metadata(prompt_id) {
            self.store
                .save_metadata(prompt_id, prompt_metadata.title, !prompt_metadata.default)
                .detach_and_log_err(cx);
            self.picker
                .update(cx, |picker, cx| picker.refresh(window, cx));
            cx.notify();
        }
    }

    pub fn load_prompt(
        &mut self,
        prompt_id: PromptId,
        focus: bool,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(prompt_editor) = self.prompt_editors.get(&prompt_id) {
            if focus {
                prompt_editor
                    .body_editor
                    .update(cx, |editor, cx| window.focus(&editor.focus_handle(cx)));
            }
            self.set_active_prompt(Some(prompt_id), window, cx);
        } else if let Some(prompt_metadata) = self.store.metadata(prompt_id) {
            let language_registry = self.language_registry.clone();
            let prompt = self.store.load(prompt_id);
            self.pending_load = cx.spawn_in(window, |this, mut cx| async move {
                let prompt = prompt.await;
                let markdown = language_registry.language_for_name("Markdown").await;
                this.update_in(&mut cx, |this, window, cx| match prompt {
                    Ok(prompt) => {
                        let title_editor = cx.new_model(|cx| {
                            let mut editor = Editor::auto_width(window, cx);
                            editor.set_placeholder_text("Untitled", cx);
                            editor.set_text(prompt_metadata.title.unwrap_or_default(), window, cx);
                            if prompt_id.is_built_in() {
                                editor.set_read_only(true);
                                editor.set_show_inline_completions(Some(false), window, cx);
                            }
                            editor
                        });
                        let body_editor = cx.new_model(|cx| {
                            let buffer = cx.new_model(|cx| {
                                let mut buffer = Buffer::local(prompt, cx);
                                buffer.set_language(markdown.log_err(), cx);
                                buffer.set_language_registry(language_registry);
                                buffer
                            });

                            let mut editor = Editor::for_buffer(buffer, None, window, cx);
                            if prompt_id.is_built_in() {
                                editor.set_read_only(true);
                                editor.set_show_inline_completions(Some(false), window, cx);
                            }
                            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                            editor.set_show_gutter(false, cx);
                            editor.set_show_wrap_guides(false, cx);
                            editor.set_show_indent_guides(false, cx);
                            editor.set_use_modal_editing(false);
                            editor.set_current_line_highlight(Some(CurrentLineHighlight::None));
                            editor.set_completion_provider(Some(Box::new(
                                SlashCommandCompletionProvider::new(
                                    Arc::new(SlashCommandWorkingSet::default()),
                                    None,
                                    None,
                                ),
                            )));
                            if focus {
                                window.focus(&editor.focus_handle(cx));
                            }
                            editor
                        });
                        let _subscriptions = vec![
                            cx.subscribe_in(
                                &title_editor,
                                window,
                                move |this, editor, event, window, cx| {
                                    this.handle_prompt_title_editor_event(
                                        prompt_id, editor, event, window, cx,
                                    )
                                },
                            ),
                            cx.subscribe_in(
                                &body_editor,
                                window,
                                move |this, editor, event, window, cx| {
                                    this.handle_prompt_body_editor_event(
                                        prompt_id, editor, event, window, cx,
                                    )
                                },
                            ),
                        ];
                        this.prompt_editors.insert(
                            prompt_id,
                            PromptEditor {
                                title_editor,
                                body_editor,
                                next_title_and_body_to_save: None,
                                pending_save: None,
                                token_count: None,
                                pending_token_count: Task::ready(None),
                                _subscriptions,
                            },
                        );
                        this.set_active_prompt(Some(prompt_id), window, cx);
                        this.count_tokens(prompt_id, window, cx);
                    }
                    Err(error) => {
                        // TODO: we should show the error in the UI.
                        log::error!("error while loading prompt: {:?}", error);
                    }
                })
                .ok();
            });
        }
    }

    fn set_active_prompt(
        &mut self,
        prompt_id: Option<PromptId>,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        self.active_prompt_id = prompt_id;
        self.picker.update(cx, |picker, cx| {
            if let Some(prompt_id) = prompt_id {
                if picker
                    .delegate
                    .matches
                    .get(picker.delegate.selected_index())
                    .map_or(true, |old_selected_prompt| {
                        old_selected_prompt.id != prompt_id
                    })
                {
                    if let Some(ix) = picker
                        .delegate
                        .matches
                        .iter()
                        .position(|mat| mat.id == prompt_id)
                    {
                        picker.set_selected_index(ix, true, window, cx);
                    }
                }
            } else {
                picker.focus(window, cx);
            }
        });
        cx.notify();
    }

    pub fn delete_prompt(
        &mut self,
        prompt_id: PromptId,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(metadata) = self.store.metadata(prompt_id) {
            let confirmation = window.prompt(
                PromptLevel::Warning,
                &format!(
                    "Are you sure you want to delete {}",
                    metadata.title.unwrap_or("Untitled".into())
                ),
                None,
                &["Delete", "Cancel"],
                cx,
            );

            cx.spawn_in(window, |this, mut cx| async move {
                if confirmation.await.ok() == Some(0) {
                    this.update_in(&mut cx, |this, window, cx| {
                        if this.active_prompt_id == Some(prompt_id) {
                            this.set_active_prompt(None, window, cx);
                        }
                        this.prompt_editors.remove(&prompt_id);
                        this.store.delete(prompt_id).detach_and_log_err(cx);
                        this.picker
                            .update(cx, |picker, cx| picker.refresh(window, cx));
                        cx.notify();
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
    }

    pub fn duplicate_prompt(
        &mut self,
        prompt_id: PromptId,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(prompt) = self.prompt_editors.get(&prompt_id) {
            const DUPLICATE_SUFFIX: &str = " copy";
            let title_to_duplicate = prompt.title_editor.read(cx).text(cx);
            let existing_titles = self
                .prompt_editors
                .iter()
                .filter(|&(&id, _)| id != prompt_id)
                .map(|(_, prompt_editor)| prompt_editor.title_editor.read(cx).text(cx))
                .filter(|title| title.starts_with(&title_to_duplicate))
                .collect::<HashSet<_>>();

            let title = if existing_titles.is_empty() {
                title_to_duplicate + DUPLICATE_SUFFIX
            } else {
                let mut i = 1;
                loop {
                    let new_title = format!("{title_to_duplicate}{DUPLICATE_SUFFIX} {i}");
                    if !existing_titles.contains(&new_title) {
                        break new_title;
                    }
                    i += 1;
                }
            };

            let new_id = PromptId::new();
            let body = prompt.body_editor.read(cx).text(cx);
            let save = self
                .store
                .save(new_id, Some(title.into()), false, body.into());
            self.picker
                .update(cx, |picker, cx| picker.refresh(window, cx));
            cx.spawn_in(window, |this, mut cx| async move {
                save.await?;
                this.update_in(&mut cx, |prompt_library, window, cx| {
                    prompt_library.load_prompt(new_id, true, window, cx)
                })
            })
            .detach_and_log_err(cx);
        }
    }

    fn focus_active_prompt(&mut self, _: &Tab, window: &mut Window, cx: &mut ModelContext<Self>) {
        if let Some(active_prompt) = self.active_prompt_id {
            self.prompt_editors[&active_prompt]
                .body_editor
                .update(cx, |editor, cx| window.focus(&editor.focus_handle(cx)));
            cx.stop_propagation();
        }
    }

    fn focus_picker(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut ModelContext<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.focus(window, cx));
    }

    pub fn inline_assist(
        &mut self,
        action: &InlineAssist,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let Some(active_prompt_id) = self.active_prompt_id else {
            cx.propagate();
            return;
        };

        let prompt_editor = &self.prompt_editors[&active_prompt_id].body_editor;
        let Some(provider) = LanguageModelRegistry::read_global(cx).active_provider() else {
            return;
        };

        let initial_prompt = action.prompt.clone();
        if provider.is_authenticated(cx) {
            InlineAssistant::update_global(cx, |assistant, cx| {
                assistant.assist(&prompt_editor, None, None, initial_prompt, window, cx)
            })
        } else {
            for window in cx.windows() {
                if let Some(workspace) = window.downcast::<Workspace>() {
                    let panel = workspace
                        .update(cx, |workspace, window, cx| {
                            window.activate_window();
                            workspace.focus_panel::<AssistantPanel>(window, cx)
                        })
                        .ok()
                        .flatten();
                    if panel.is_some() {
                        return;
                    }
                }
            }
        }
    }

    fn move_down_from_title(
        &mut self,
        _: &editor::actions::MoveDown,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(prompt_id) = self.active_prompt_id {
            if let Some(prompt_editor) = self.prompt_editors.get(&prompt_id) {
                window.focus(&prompt_editor.body_editor.focus_handle(cx));
            }
        }
    }

    fn move_up_from_body(
        &mut self,
        _: &editor::actions::MoveUp,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let Some(prompt_id) = self.active_prompt_id {
            if let Some(prompt_editor) = self.prompt_editors.get(&prompt_id) {
                window.focus(&prompt_editor.title_editor.focus_handle(cx));
            }
        }
    }

    fn handle_prompt_title_editor_event(
        &mut self,
        prompt_id: PromptId,
        title_editor: &Model<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                self.save_prompt(prompt_id, window, cx);
                self.count_tokens(prompt_id, window, cx);
            }
            EditorEvent::Blurred => {
                title_editor.update(cx, |title_editor, cx| {
                    title_editor.change_selections(None, window, cx, |selections| {
                        let cursor = selections.oldest_anchor().head();
                        selections.select_anchor_ranges([cursor..cursor]);
                    });
                });
            }
            _ => {}
        }
    }

    fn handle_prompt_body_editor_event(
        &mut self,
        prompt_id: PromptId,
        body_editor: &Model<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        match event {
            EditorEvent::BufferEdited => {
                self.save_prompt(prompt_id, window, cx);
                self.count_tokens(prompt_id, window, cx);
            }
            EditorEvent::Blurred => {
                body_editor.update(cx, |body_editor, cx| {
                    body_editor.change_selections(None, window, cx, |selections| {
                        let cursor = selections.oldest_anchor().head();
                        selections.select_anchor_ranges([cursor..cursor]);
                    });
                });
            }
            _ => {}
        }
    }

    fn count_tokens(
        &mut self,
        prompt_id: PromptId,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let Some(model) = LanguageModelRegistry::read_global(cx).active_model() else {
            return;
        };
        if let Some(prompt) = self.prompt_editors.get_mut(&prompt_id) {
            let editor = &prompt.body_editor.read(cx);
            let buffer = &editor.buffer().read(cx).as_singleton().unwrap().read(cx);
            let body = buffer.as_rope().clone();
            prompt.pending_token_count = cx.spawn_in(window, |this, mut cx| {
                async move {
                    const DEBOUNCE_TIMEOUT: Duration = Duration::from_secs(1);

                    cx.background_executor().timer(DEBOUNCE_TIMEOUT).await;
                    let token_count = cx
                        .update(|_, cx| {
                            model.count_tokens(
                                LanguageModelRequest {
                                    messages: vec![LanguageModelRequestMessage {
                                        role: Role::System,
                                        content: vec![body.to_string().into()],
                                        cache: false,
                                    }],
                                    tools: Vec::new(),
                                    stop: Vec::new(),
                                    temperature: None,
                                },
                                cx,
                            )
                        })?
                        .await?;

                    this.update(&mut cx, |this, cx| {
                        let prompt_editor = this.prompt_editors.get_mut(&prompt_id).unwrap();
                        prompt_editor.token_count = Some(token_count);
                        cx.notify();
                    })
                }
                .log_err()
            });
        }
    }

    fn render_prompt_list(&mut self, cx: &mut ModelContext<Self>) -> impl IntoElement {
        v_flex()
            .id("prompt-list")
            .capture_action(cx.listener(Self::focus_active_prompt))
            .bg(cx.theme().colors().panel_background)
            .h_full()
            .px_1()
            .w_1_3()
            .overflow_x_hidden()
            .child(
                h_flex()
                    .p(DynamicSpacing::Base04.rems(cx))
                    .h_9()
                    .w_full()
                    .flex_none()
                    .justify_end()
                    .child(
                        IconButton::new("new-prompt", IconName::Plus)
                            .style(ButtonStyle::Transparent)
                            .shape(IconButtonShape::Square)
                            .tooltip(move |window, cx| {
                                Tooltip::for_action("New Prompt", &NewPrompt, window, cx)
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(NewPrompt), cx);
                            }),
                    ),
            )
            .child(div().flex_grow().child(self.picker.clone()))
    }

    fn render_active_prompt(
        &mut self,
        cx: &mut ModelContext<PromptLibrary>,
    ) -> gpui::Stateful<Div> {
        div()
            .w_2_3()
            .h_full()
            .id("prompt-editor")
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .flex_none()
            .min_w_64()
            .children(self.active_prompt_id.and_then(|prompt_id| {
                let prompt_metadata = self.store.metadata(prompt_id)?;
                let prompt_editor = &self.prompt_editors[&prompt_id];
                let focus_handle = prompt_editor.body_editor.focus_handle(cx);
                let model = LanguageModelRegistry::read_global(cx).active_model();
                let settings = ThemeSettings::get_global(cx);

                Some(
                    v_flex()
                        .id("prompt-editor-inner")
                        .size_full()
                        .relative()
                        .overflow_hidden()
                        .pl(DynamicSpacing::Base16.rems(cx))
                        .pt(DynamicSpacing::Base08.rems(cx))
                        .on_click(cx.listener(move |_, _, window, _| {
                            window.focus(&focus_handle);
                        }))
                        .child(
                            h_flex()
                                .group("active-editor-header")
                                .pr(DynamicSpacing::Base16.rems(cx))
                                .pt(DynamicSpacing::Base02.rems(cx))
                                .pb(DynamicSpacing::Base08.rems(cx))
                                .justify_between()
                                .child(
                                    h_flex().gap_1().child(
                                        div()
                                            .max_w_80()
                                            .on_action(cx.listener(Self::move_down_from_title))
                                            .border_1()
                                            .border_color(transparent_black())
                                            .rounded_md()
                                            .group_hover("active-editor-header", |this| {
                                                this.border_color(
                                                    cx.theme().colors().border_variant,
                                                )
                                            })
                                            .child(EditorElement::new(
                                                &prompt_editor.title_editor,
                                                EditorStyle {
                                                    background: cx.theme().system().transparent,
                                                    local_player: cx.theme().players().local(),
                                                    text: TextStyle {
                                                        color: cx
                                                            .theme()
                                                            .colors()
                                                            .editor_foreground,
                                                        font_family: settings
                                                            .ui_font
                                                            .family
                                                            .clone(),
                                                        font_features: settings
                                                            .ui_font
                                                            .features
                                                            .clone(),
                                                        font_size: HeadlineSize::Large
                                                            .rems()
                                                            .into(),
                                                        font_weight: settings.ui_font.weight,
                                                        line_height: relative(
                                                            settings.buffer_line_height.value(),
                                                        ),
                                                        ..Default::default()
                                                    },
                                                    scrollbar_width: Pixels::ZERO,
                                                    syntax: cx.theme().syntax().clone(),
                                                    status: cx.theme().status().clone(),
                                                    inlay_hints_style:
                                                        editor::make_inlay_hints_style(cx),
                                                    inline_completion_styles:
                                                        editor::make_suggestion_styles(cx),
                                                    ..EditorStyle::default()
                                                },
                                            )),
                                    ),
                                )
                                .child(
                                    h_flex()
                                        .h_full()
                                        .child(
                                            h_flex()
                                                .h_full()
                                                .gap(DynamicSpacing::Base16.rems(cx))
                                                .child(div()),
                                        )
                                        .child(
                                            h_flex()
                                                .h_full()
                                                .gap(DynamicSpacing::Base16.rems(cx))
                                                .children(prompt_editor.token_count.map(
                                                    |token_count| {
                                                        let token_count: SharedString =
                                                            token_count.to_string().into();
                                                        let label_token_count: SharedString =
                                                            token_count.to_string().into();

                                                        h_flex()
                                                            .id("token_count")
                                                            .tooltip(move |window, cx| {
                                                                let token_count =
                                                                    token_count.clone();

                                                                Tooltip::with_meta(
                                                                    format!(
                                                                        "{} tokens",
                                                                        token_count.clone()
                                                                    ),
                                                                    None,
                                                                    format!(
                                                                        "Model: {}",
                                                                        model
                                                                            .as_ref()
                                                                            .map(|model| model
                                                                                .name()
                                                                                .0)
                                                                            .unwrap_or_default()
                                                                    ),
                                                                    window,
                                                                    cx,
                                                                )
                                                            })
                                                            .child(
                                                                Label::new(format!(
                                                                    "{} tokens",
                                                                    label_token_count.clone()
                                                                ))
                                                                .color(Color::Muted),
                                                            )
                                                    },
                                                ))
                                                .child(if prompt_id.is_built_in() {
                                                    div()
                                                        .id("built-in-prompt")
                                                        .child(
                                                            Icon::new(IconName::FileLock)
                                                                .color(Color::Muted),
                                                        )
                                                        .tooltip(move |window, cx| {
                                                            Tooltip::with_meta(
                                                                "Built-in prompt",
                                                                None,
                                                                BUILT_IN_TOOLTIP_TEXT,
                                                                window,
                                                                cx,
                                                            )
                                                        })
                                                        .into_any()
                                                } else {
                                                    IconButton::new(
                                                        "delete-prompt",
                                                        IconName::Trash,
                                                    )
                                                    .size(ButtonSize::Large)
                                                    .style(ButtonStyle::Transparent)
                                                    .shape(IconButtonShape::Square)
                                                    .size(ButtonSize::Large)
                                                    .tooltip(move |window, cx| {
                                                        Tooltip::for_action(
                                                            "Delete Prompt",
                                                            &DeletePrompt,
                                                            window,
                                                            cx,
                                                        )
                                                    })
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(
                                                            Box::new(DeletePrompt),
                                                            cx,
                                                        );
                                                    })
                                                    .into_any_element()
                                                })
                                                .child(
                                                    IconButton::new(
                                                        "duplicate-prompt",
                                                        IconName::BookCopy,
                                                    )
                                                    .size(ButtonSize::Large)
                                                    .style(ButtonStyle::Transparent)
                                                    .shape(IconButtonShape::Square)
                                                    .size(ButtonSize::Large)
                                                    .tooltip(move |window, cx| {
                                                        Tooltip::for_action(
                                                            "Duplicate Prompt",
                                                            &DuplicatePrompt,
                                                            window,
                                                            cx,
                                                        )
                                                    })
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(
                                                            Box::new(DuplicatePrompt),
                                                            cx,
                                                        );
                                                    }),
                                                )
                                                .child(
                                                    IconButton::new(
                                                        "toggle-default-prompt",
                                                        IconName::Sparkle,
                                                    )
                                                    .style(ButtonStyle::Transparent)
                                                    .toggle_state(prompt_metadata.default)
                                                    .selected_icon(IconName::SparkleFilled)
                                                    .icon_color(if prompt_metadata.default {
                                                        Color::Accent
                                                    } else {
                                                        Color::Muted
                                                    })
                                                    .shape(IconButtonShape::Square)
                                                    .size(ButtonSize::Large)
                                                    .tooltip(Tooltip::text(
                                                        if prompt_metadata.default {
                                                            "Remove from Default Prompt"
                                                        } else {
                                                            "Add to Default Prompt"
                                                        },
                                                    ))
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(
                                                            Box::new(ToggleDefaultPrompt),
                                                            cx,
                                                        );
                                                    }),
                                                ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .on_action(cx.listener(Self::focus_picker))
                                .on_action(cx.listener(Self::inline_assist))
                                .on_action(cx.listener(Self::move_up_from_body))
                                .flex_grow()
                                .h_full()
                                .child(prompt_editor.body_editor.clone()),
                        ),
                )
            }))
    }
}

impl Render for PromptLibrary {
    fn render(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) -> impl IntoElement {
        let ui_font = theme::setup_ui_font(window, cx);
        let theme = cx.theme().clone();

        h_flex()
            .id("prompt-manager")
            .key_context("PromptLibrary")
            .on_action(cx.listener(|this, &NewPrompt, window, cx| this.new_prompt(window, cx)))
            .on_action(
                cx.listener(|this, &DeletePrompt, window, cx| {
                    this.delete_active_prompt(window, cx)
                }),
            )
            .on_action(cx.listener(|this, &DuplicatePrompt, window, cx| {
                this.duplicate_active_prompt(window, cx)
            }))
            .on_action(cx.listener(|this, &ToggleDefaultPrompt, window, cx| {
                this.toggle_default_for_active_prompt(window, cx)
            }))
            .size_full()
            .overflow_hidden()
            .font(ui_font)
            .text_color(theme.colors().text)
            .child(self.render_prompt_list(cx))
            .map(|el| {
                if self.store.prompt_count() == 0 {
                    el.child(
                        v_flex()
                            .w_2_3()
                            .h_full()
                            .items_center()
                            .justify_center()
                            .gap_4()
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Icon::new(IconName::Book)
                                            .size(IconSize::Medium)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new("No prompts yet")
                                            .size(LabelSize::Large)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .child(h_flex())
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(Label::new("Create your first prompt:"))
                                            .child(
                                                Button::new("create-prompt", "New Prompt")
                                                    .full_width()
                                                    .key_binding(KeyBinding::for_action(
                                                        &NewPrompt, window,
                                                    ))
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(
                                                            NewPrompt.boxed_clone(),
                                                            cx,
                                                        )
                                                    }),
                                            ),
                                    )
                                    .child(h_flex()),
                            ),
                    )
                } else {
                    el.child(self.render_active_prompt(cx))
                }
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptMetadata {
    pub id: PromptId,
    pub title: Option<SharedString>,
    pub default: bool,
    pub saved_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PromptId {
    User { uuid: Uuid },
    EditWorkflow,
}

impl PromptId {
    pub fn new() -> PromptId {
        PromptId::User {
            uuid: Uuid::new_v4(),
        }
    }

    pub fn is_built_in(&self) -> bool {
        !matches!(self, PromptId::User { .. })
    }
}

pub struct PromptStore {
    executor: BackgroundExecutor,
    env: heed::Env,
    metadata_cache: RwLock<MetadataCache>,
    metadata: Database<SerdeJson<PromptId>, SerdeJson<PromptMetadata>>,
    bodies: Database<SerdeJson<PromptId>, Str>,
}

#[derive(Default)]
struct MetadataCache {
    metadata: Vec<PromptMetadata>,
    metadata_by_id: HashMap<PromptId, PromptMetadata>,
}

impl MetadataCache {
    fn from_db(
        db: Database<SerdeJson<PromptId>, SerdeJson<PromptMetadata>>,
        txn: &RoTxn,
    ) -> Result<Self> {
        let mut cache = MetadataCache::default();
        for result in db.iter(txn)? {
            let (prompt_id, metadata) = result?;
            cache.metadata.push(metadata.clone());
            cache.metadata_by_id.insert(prompt_id, metadata);
        }
        cache.sort();
        Ok(cache)
    }

    fn insert(&mut self, metadata: PromptMetadata) {
        self.metadata_by_id.insert(metadata.id, metadata.clone());
        if let Some(old_metadata) = self.metadata.iter_mut().find(|m| m.id == metadata.id) {
            *old_metadata = metadata;
        } else {
            self.metadata.push(metadata);
        }
        self.sort();
    }

    fn remove(&mut self, id: PromptId) {
        self.metadata.retain(|metadata| metadata.id != id);
        self.metadata_by_id.remove(&id);
    }

    fn sort(&mut self) {
        self.metadata.sort_unstable_by(|a, b| {
            a.title
                .cmp(&b.title)
                .then_with(|| b.saved_at.cmp(&a.saved_at))
        });
    }
}

impl PromptStore {
    pub fn global(cx: &AppContext) -> impl Future<Output = Result<Arc<Self>>> {
        let store = GlobalPromptStore::global(cx).0.clone();
        async move { store.await.map_err(|err| anyhow!(err)) }
    }

    pub fn new(db_path: PathBuf, executor: BackgroundExecutor) -> Task<Result<Self>> {
        executor.spawn({
            let executor = executor.clone();
            async move {
                std::fs::create_dir_all(&db_path)?;

                let db_env = unsafe {
                    heed::EnvOpenOptions::new()
                        .map_size(1024 * 1024 * 1024) // 1GB
                        .max_dbs(4) // Metadata and bodies (possibly v1 of both as well)
                        .open(db_path)?
                };

                let mut txn = db_env.write_txn()?;
                let metadata = db_env.create_database(&mut txn, Some("metadata.v2"))?;
                let bodies = db_env.create_database(&mut txn, Some("bodies.v2"))?;

                // Remove edit workflow prompt, as we decided to opt into it using
                // a slash command instead.
                metadata.delete(&mut txn, &PromptId::EditWorkflow).ok();
                bodies.delete(&mut txn, &PromptId::EditWorkflow).ok();

                txn.commit()?;

                Self::upgrade_dbs(&db_env, metadata, bodies).log_err();

                let txn = db_env.read_txn()?;
                let metadata_cache = MetadataCache::from_db(metadata, &txn)?;
                txn.commit()?;

                Ok(PromptStore {
                    executor,
                    env: db_env,
                    metadata_cache: RwLock::new(metadata_cache),
                    metadata,
                    bodies,
                })
            }
        })
    }

    fn upgrade_dbs(
        env: &heed::Env,
        metadata_db: heed::Database<SerdeJson<PromptId>, SerdeJson<PromptMetadata>>,
        bodies_db: heed::Database<SerdeJson<PromptId>, Str>,
    ) -> Result<()> {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
        pub struct PromptIdV1(Uuid);

        #[derive(Clone, Debug, Serialize, Deserialize)]
        pub struct PromptMetadataV1 {
            pub id: PromptIdV1,
            pub title: Option<SharedString>,
            pub default: bool,
            pub saved_at: DateTime<Utc>,
        }

        let mut txn = env.write_txn()?;
        let Some(bodies_v1_db) = env
            .open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<String>>(
                &txn,
                Some("bodies"),
            )?
        else {
            return Ok(());
        };
        let mut bodies_v1 = bodies_v1_db
            .iter(&txn)?
            .collect::<heed::Result<HashMap<_, _>>>()?;

        let Some(metadata_v1_db) = env
            .open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<PromptMetadataV1>>(
                &txn,
                Some("metadata"),
            )?
        else {
            return Ok(());
        };
        let metadata_v1 = metadata_v1_db
            .iter(&txn)?
            .collect::<heed::Result<HashMap<_, _>>>()?;

        for (prompt_id_v1, metadata_v1) in metadata_v1 {
            let prompt_id_v2 = PromptId::User {
                uuid: prompt_id_v1.0,
            };
            let Some(body_v1) = bodies_v1.remove(&prompt_id_v1) else {
                continue;
            };

            if metadata_db
                .get(&txn, &prompt_id_v2)?
                .map_or(true, |metadata_v2| {
                    metadata_v1.saved_at > metadata_v2.saved_at
                })
            {
                metadata_db.put(
                    &mut txn,
                    &prompt_id_v2,
                    &PromptMetadata {
                        id: prompt_id_v2,
                        title: metadata_v1.title.clone(),
                        default: metadata_v1.default,
                        saved_at: metadata_v1.saved_at,
                    },
                )?;
                bodies_db.put(&mut txn, &prompt_id_v2, &body_v1)?;
            }
        }

        txn.commit()?;

        Ok(())
    }

    pub fn load(&self, id: PromptId) -> Task<Result<String>> {
        let env = self.env.clone();
        let bodies = self.bodies;
        self.executor.spawn(async move {
            let txn = env.read_txn()?;
            let mut prompt = bodies
                .get(&txn, &id)?
                .ok_or_else(|| anyhow!("prompt not found"))?
                .into();
            LineEnding::normalize(&mut prompt);
            Ok(prompt)
        })
    }

    pub fn default_prompt_metadata(&self) -> Vec<PromptMetadata> {
        return self
            .metadata_cache
            .read()
            .metadata
            .iter()
            .filter(|metadata| metadata.default)
            .cloned()
            .collect::<Vec<_>>();
    }

    pub fn delete(&self, id: PromptId) -> Task<Result<()>> {
        self.metadata_cache.write().remove(id);

        let db_connection = self.env.clone();
        let bodies = self.bodies;
        let metadata = self.metadata;

        self.executor.spawn(async move {
            let mut txn = db_connection.write_txn()?;

            metadata.delete(&mut txn, &id)?;
            bodies.delete(&mut txn, &id)?;

            txn.commit()?;
            Ok(())
        })
    }

    /// Returns the number of prompts in the store.
    fn prompt_count(&self) -> usize {
        self.metadata_cache.read().metadata.len()
    }

    fn metadata(&self, id: PromptId) -> Option<PromptMetadata> {
        self.metadata_cache.read().metadata_by_id.get(&id).cloned()
    }

    pub fn id_for_title(&self, title: &str) -> Option<PromptId> {
        let metadata_cache = self.metadata_cache.read();
        let metadata = metadata_cache
            .metadata
            .iter()
            .find(|metadata| metadata.title.as_ref().map(|title| &***title) == Some(title))?;
        Some(metadata.id)
    }

    pub fn search(&self, query: String) -> Task<Vec<PromptMetadata>> {
        let cached_metadata = self.metadata_cache.read().metadata.clone();
        let executor = self.executor.clone();
        self.executor.spawn(async move {
            let mut matches = if query.is_empty() {
                cached_metadata
            } else {
                let candidates = cached_metadata
                    .iter()
                    .enumerate()
                    .filter_map(|(ix, metadata)| {
                        Some(StringMatchCandidate::new(ix, metadata.title.as_ref()?))
                    })
                    .collect::<Vec<_>>();
                let matches = fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    100,
                    &AtomicBool::default(),
                    executor,
                )
                .await;
                matches
                    .into_iter()
                    .map(|mat| cached_metadata[mat.candidate_id].clone())
                    .collect()
            };
            matches.sort_by_key(|metadata| Reverse(metadata.default));
            matches
        })
    }

    fn save(
        &self,
        id: PromptId,
        title: Option<SharedString>,
        default: bool,
        body: Rope,
    ) -> Task<Result<()>> {
        if id.is_built_in() {
            return Task::ready(Err(anyhow!("built-in prompts cannot be saved")));
        }

        let prompt_metadata = PromptMetadata {
            id,
            title,
            default,
            saved_at: Utc::now(),
        };
        self.metadata_cache.write().insert(prompt_metadata.clone());

        let db_connection = self.env.clone();
        let bodies = self.bodies;
        let metadata = self.metadata;

        self.executor.spawn(async move {
            let mut txn = db_connection.write_txn()?;

            metadata.put(&mut txn, &id, &prompt_metadata)?;
            bodies.put(&mut txn, &id, &body.to_string())?;

            txn.commit()?;

            Ok(())
        })
    }

    fn save_metadata(
        &self,
        id: PromptId,
        mut title: Option<SharedString>,
        default: bool,
    ) -> Task<Result<()>> {
        let mut cache = self.metadata_cache.write();

        if id.is_built_in() {
            title = cache
                .metadata_by_id
                .get(&id)
                .and_then(|metadata| metadata.title.clone());
        }

        let prompt_metadata = PromptMetadata {
            id,
            title,
            default,
            saved_at: Utc::now(),
        };

        cache.insert(prompt_metadata.clone());

        let db_connection = self.env.clone();
        let metadata = self.metadata;

        self.executor.spawn(async move {
            let mut txn = db_connection.write_txn()?;
            metadata.put(&mut txn, &id, &prompt_metadata)?;
            txn.commit()?;

            Ok(())
        })
    }

    fn first(&self) -> Option<PromptMetadata> {
        self.metadata_cache.read().metadata.first().cloned()
    }
}

/// Wraps a shared future to a prompt store so it can be assigned as a context global.
pub struct GlobalPromptStore(
    Shared<BoxFuture<'static, Result<Arc<PromptStore>, Arc<anyhow::Error>>>>,
);

impl Global for GlobalPromptStore {}
