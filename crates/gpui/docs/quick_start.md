# GPUI Quick Start Guide

Get up and running with GPUI in minutes!

## Installation

### 1. Prerequisites

**macOS:**
```bash
xcode-select --install
```

**Linux (Ubuntu/Debian):**
```bash
sudo apt install libssl-dev pkg-config libfontconfig-dev
```

### 2. Create a New Project

```bash
cargo new my_gpui_app
cd my_gpui_app
```

### 3. Add GPUI Dependency

Edit `Cargo.toml`:
```toml
[dependencies]
gpui = { git = "https://github.com/zed-industries/zed" }
```

## Your First App (5 Minutes)

Replace the contents of `src/main.rs`:

```rust
use gpui::{
    App, Application, Bounds, Context, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

struct MyApp {
    count: i32,
}

impl Render for MyApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .justify_center()
            .items_center()
            .child(
                div()
                    .text_3xl()
                    .text_color(rgb(0xcdd6f4))
                    .child(format!("Count: {}", self.count))
            )
            .child(
                div()
                    .px_6()
                    .py_3()
                    .bg(rgb(0x89b4fa))
                    .text_color(rgb(0x1e1e2e))
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|this| this.bg(rgb(0x74c7ec)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.count += 1;
                        cx.notify();
                    }))
                    .child("Click me!")
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(400.), px(300.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| MyApp { count: 0 }),
        )
        .unwrap();
        cx.activate(true);
    });
}
```

### Run Your App

```bash
cargo run
```

You should see a window with a counter and a button!

## What's Happening?

1. **Application**: `Application::new().run()` starts your app
2. **Window**: `cx.open_window()` creates a window
3. **View**: `MyApp` is your root view that implements `Render`
4. **Elements**: `div()` creates UI elements with Tailwind-like styling
5. **Interactivity**: `.on_click()` handles user clicks
6. **State Updates**: `cx.notify()` triggers re-renders

## Next Steps

### Add More Interactivity

```rust
// Add a decrement button
.child(
    div()
        .px_6()
        .py_3()
        .bg(rgb(0xf38ba8))
        .text_color(rgb(0x1e1e2e))
        .rounded_lg()
        .cursor_pointer()
        .hover(|this| this.bg(rgb(0xeba0ac)))
        .on_click(cx.listener(|this, _, _, cx| {
            this.count -= 1;
            cx.notify();
        }))
        .child("Decrement")
)
```

### Add Keyboard Shortcuts

```rust
use gpui::actions;

actions!(my_app, [Increment, Decrement]);

impl MyApp {
    fn increment(&mut self, _: &Increment, _: &mut Window, cx: &mut Context<Self>) {
        self.count += 1;
        cx.notify();
    }
}

// In render():
div()
    .key_context("MyApp")
    .on_action(cx.listener(Self::increment))
    // ... rest of UI
```

### Style Your App

```rust
// Change colors
.bg(rgb(0x2e3440))           // Nord Polar Night
.text_color(rgb(0xeceff4))   // Nord Snow Storm

// Add spacing
.p_8()      // padding
.gap_6()    // gap between children

// Add borders
.border_2()
.border_color(rgb(0x4c566a))
.rounded_xl()

// Add shadows
.shadow_lg()
```

### Learn More

- [Beginner's Guide](beginner_guide.md) - Complete tutorial
- [Examples Reference](examples_reference.md) - All examples explained
- [Contexts](contexts.md) - Understanding contexts
- [Key Dispatch](key_dispatch.md) - Keyboard actions

## Common Patterns

### Layout a Form

```rust
div()
    .flex()
    .flex_col()
    .gap_4()
    .p_6()
    .child(label("Name"))
    .child(input_field())
    .child(label("Email"))
    .child(input_field())
    .child(submit_button())
```

### Create a Card

```rust
div()
    .p_6()
    .bg(rgb(0x3b4252))
    .rounded_lg()
    .shadow_md()
    .child(title())
    .child(content())
```

### Build a List

```rust
let items = vec!["Item 1", "Item 2", "Item 3"];

div()
    .flex()
    .flex_col()
    .gap_2()
    .children(items.iter().map(|item| {
        div()
            .p_3()
            .bg(rgb(0x3b4252))
            .rounded_md()
            .hover(|this| this.bg(rgb(0x434c5e)))
            .child(*item)
    }))
```

## Troubleshooting

### "No such module" error
Make sure you've added GPUI to your `Cargo.toml`.

### Window doesn't appear
Ensure you call `cx.activate(true)` after creating the window.

### Changes don't show
Did you call `cx.notify()` after updating state?

### Compile errors with styling
Check that you're using the `prelude::*` import for element extensions.

## Resources

- [GPUI Repository](https://github.com/zed-industries/zed) - Source code and examples
- [Zed Discord](https://zed.dev/community-links) - Get help from the community
- [gpui.rs](https://www.gpui.rs/) - Interactive documentation

Happy building! 🚀
