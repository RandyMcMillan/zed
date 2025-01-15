use gpui::{actions, AppContext, ClipboardItem, PromptLevel};
use system_specs::SystemSpecs;
use util::ResultExt;
use workspace::Workspace;

pub mod feedback_modal;

mod system_specs;

actions!(
    zed,
    [
        CopySystemSpecsIntoClipboard,
        FileBugReport,
        RequestFeature,
        OpenZedRepo
    ]
);

const fn zed_repo_url() -> &'static str {
    "https://github.com/zed-industries/zed"
}

fn request_feature_url(specs: &SystemSpecs) -> String {
    format!(
        concat!(
            "https://github.com/zed-industries/zed/issues/new",
            "?labels=admin+read%2Ctriage%2Cenhancement",
            "&template=0_feature_request.yml",
            "&environment={}"
        ),
        urlencoding::encode(&specs.to_string())
    )
}

fn file_bug_report_url(specs: &SystemSpecs) -> String {
    format!(
        concat!(
            "https://github.com/zed-industries/zed/issues/new",
            "?labels=admin+read%2Ctriage%2Cbug",
            "&template=1_bug_report.yml",
            "&environment={}"
        ),
        urlencoding::encode(&specs.to_string())
    )
}

pub fn init(cx: &mut AppContext) {
    cx.observe_new_window_models(|workspace: &mut Workspace, window, cx| {
        feedback_modal::FeedbackModal::register(workspace, window, cx);
        workspace
            .register_action(|_, _: &CopySystemSpecsIntoClipboard, window, cx| {
                let specs = SystemSpecs::new(window, cx);

                cx.spawn_in(window, |_, mut cx| async move {
                    let specs = specs.await.to_string();

                    cx.update(|_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(specs.clone()))
                    })
                    .log_err();

                    cx.prompt(
                        PromptLevel::Info,
                        "Copied into clipboard",
                        Some(&specs),
                        &["OK"],
                    )
                    .await
                    .ok();
                })
                .detach();
            })
            .register_action(|_, _: &RequestFeature, window, cx| {
                let specs = SystemSpecs::new(window, cx);
                cx.spawn_in(window, |_, mut cx| async move {
                    let specs = specs.await;
                    cx.update(|_, cx| {
                        cx.open_url(&request_feature_url(&specs));
                    })
                    .log_err();
                })
                .detach();
            })
            .register_action(move |_, _: &FileBugReport, window, cx| {
                let specs = SystemSpecs::new(window, cx);
                cx.spawn_in(window, |_, mut cx| async move {
                    let specs = specs.await;
                    cx.update(|_, cx| {
                        cx.open_url(&file_bug_report_url(&specs));
                    })
                    .log_err();
                })
                .detach();
            })
            .register_action(move |_, _: &OpenZedRepo, _, cx| {
                cx.open_url(zed_repo_url());
            });
    })
    .detach();
}
