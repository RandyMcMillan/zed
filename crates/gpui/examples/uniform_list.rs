use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, AppContext, Bounds, ModelContext, Window,
    WindowBounds, WindowOptions,
};

struct UniformListExample {}

impl Render for UniformListExample {
    fn render(&mut self, _window: &mut Window, cx: &mut ModelContext<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0xffffff)).child(
            uniform_list(
                cx.model().clone(),
                "entries",
                50,
                |_this, range, _window, _cx| {
                    let mut items = Vec::new();
                    for ix in range {
                        let item = ix + 1;

                        items.push(
                            div()
                                .id(ix)
                                .px_2()
                                .cursor_pointer()
                                .on_click(move |_event, _window, _cx| {
                                    println!("clicked Item {item:?}");
                                })
                                .child(format!("Item {item}")),
                        );
                    }
                    items
                },
            )
            .h_full(),
        )
    }
}

fn main() {
    App::new().run(|cx: &mut AppContext| {
        let bounds = Bounds::centered(None, size(px(300.0), px(300.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new_model(|_| UniformListExample {}),
        )
        .unwrap();
    });
}
