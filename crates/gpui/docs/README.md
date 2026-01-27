# GPUI Documentation Index

Welcome to the GPUI documentation! This index will help you find the right resource for your needs.

## Getting Started

### 🚀 [Quick Start Guide](quick_start.md)
**Start here if you want to build something in 5 minutes!**

Get up and running with GPUI quickly. This guide includes:
- Installation instructions
- A minimal working example
- Common patterns and troubleshooting

**Perfect for**: First-time users who want to see results fast

---

### 📚 [Beginner's Guide](beginner_guide.md)
**Complete tutorial for learning GPUI from scratch**

A comprehensive guide covering:
- Core concepts (Application, Context, Entity, Window)
- Building user interfaces with elements
- Styling with Tailwind-like API
- State management and reactivity
- Event handling and user interaction
- Async operations and background tasks
- Testing your applications
- Best practices and patterns
- Complete example applications

**Perfect for**: Developers learning GPUI systematically

---

### 🎯 [Examples Reference](examples_reference.md)
**Guide to all GPUI examples**

Detailed descriptions of every example in the repository:
- What each example demonstrates
- Key concepts covered
- How to run the example
- Code highlights
- Learning paths (beginner → advanced)

**Perfect for**: Learning by example and reference

---

## Core Concepts

### 📖 [Contexts](contexts.md)
**Understanding GPUI's context system**

Deep dive into:
- `App` - Root context
- `Context<T>` - Entity context
- `AsyncApp` - Async context
- `Window` - Window state
- `Entity<T>` - State handles

**Perfect for**: Understanding how GPUI manages state and provides services

---

### ⌨️ [Key Dispatch](key_dispatch.md)
**Keyboard-first interactivity**

Learn about:
- Defining actions
- Binding keys to actions
- Key contexts
- Action handlers
- Keymaps

**Perfect for**: Implementing keyboard shortcuts and commands

---

## API Reference

### Core Types
- **Application** - Application entry point and event loop
- **Window** - Window state and operations
- **Entity<T>** - Managed state handles
- **Context<T>** - Entity update context
- **Element** - UI building blocks

### UI Elements
- **div()** - Primary container element
- **text()** - Text rendering
- **img()** - Image display
- **svg()** - SVG rendering

### Styling
- Layout: flexbox, grid, positioning
- Spacing: padding, margin, gap
- Colors: rgb, rgba, hsla
- Typography: fonts, sizes, weights
- Borders: width, color, radius
- Effects: shadows, opacity, transforms

### Interactive
- **on_click()** - Click handling
- **on_mouse_down/up/move()** - Mouse events
- **on_drag/on_drop()** - Drag and drop
- **on_action()** - Action handling
- **track_focus()** - Focus management

### State Management
- **Entity::read()** - Read entity state
- **Entity::update()** - Update entity state
- **cx.notify()** - Notify observers
- **cx.observe()** - Observe entities
- **cx.subscribe()** - Subscribe to events
- **cx.emit()** - Emit events

### Async Operations
- **cx.spawn()** - Foreground async task
- **cx.background_spawn()** - Background async task
- **Task<T>** - Async task handle
- **task.detach()** - Detach task

---

## Learning Paths

### Path 1: Absolute Beginner
If you're new to GPUI:

1. **[Quick Start](quick_start.md)** - Get your first app running
2. **[Beginner's Guide](beginner_guide.md)** - Read sections 1-6 (basics)
3. **[Examples: hello_world](examples_reference.md#hello_worldrs)** - Study the simplest example
4. **[Examples: text](examples_reference.md#textrs)** - Learn text rendering
5. **[Beginner's Guide](beginner_guide.md)** - Read sections 7-9 (state & events)
6. **Build a small app** - Try the Todo app from the guide

### Path 2: Building Your First Real App
You understand the basics and want to build something:

1. **[Beginner's Guide - State Management](beginner_guide.md#state-management)** - Master state
2. **[Key Dispatch](key_dispatch.md)** - Add keyboard shortcuts
3. **[Examples: input](examples_reference.md#inputrs)** - Study interactive components
4. **[Examples: data_table](examples_reference.md#data_tablers)** - Learn complex UIs
5. **[Beginner's Guide - Testing](beginner_guide.md#testing)** - Test your app
6. **Build and test your app**

### Path 3: Advanced UI Development
You're comfortable with GPUI and want to go deeper:

1. **[Examples: animation](examples_reference.md#animationrs)** - Add smooth animations
2. **[Examples: drag_drop](examples_reference.md#drag_droprs)** - Advanced interactions
3. **[Examples: uniform_list](examples_reference.md#uniform_listrs)** - Performance optimization
4. **[Examples: painting](examples_reference.md#paintingrs)** - Custom graphics
5. **Study Zed's UI codebase** - See real-world patterns
6. **Contribute to GPUI** - Help improve the framework

### Path 4: Specific Tasks

#### "I want to display data in a table"
→ [Examples: data_table](examples_reference.md#data_tablers)

#### "I need text input with selection and clipboard"
→ [Examples: input](examples_reference.md#inputrs)

#### "I want smooth animations"
→ [Examples: animation](examples_reference.md#animationrs)

#### "I need to load and display images"
→ [Examples: image_loading](examples_reference.md#image_loadingrs)

#### "I want drag and drop"
→ [Examples: drag_drop](examples_reference.md#drag_droprs)

#### "I need a tree view"
→ [Examples: tree](examples_reference.md#treers)

#### "I want keyboard shortcuts"
→ [Key Dispatch](key_dispatch.md)

#### "I need to test my UI"
→ [Examples: testing](examples_reference.md#testingrs)

---

## Quick Reference

### Creating an Application
```rust
Application::new().run(|cx: &mut App| {
    // Initialize your app
});
```

### Creating a Window
```rust
cx.open_window(WindowOptions::default(), |_, cx| {
    cx.new(|_| MyView::new())
})
```

### Implementing a View
```rust
struct MyView;

impl Render for MyView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child("Hello, GPUI!")
    }
}
```

### Updating State
```rust
fn increment(&mut self, cx: &mut Context<Self>) {
    self.count += 1;
    cx.notify(); // Trigger re-render
}
```

### Handling Events
```rust
div()
    .on_click(cx.listener(|this, event, window, cx| {
        this.handle_click();
        cx.notify();
    }))
```

### Async Operations
```rust
cx.spawn(async move |this, cx| {
    let data = fetch_data().await;
    this.update(cx, |view, cx| {
        view.data = data;
        cx.notify();
    }).ok();
}).detach();
```

---

## Additional Resources

### Official Resources
- **[GPUI Repository](https://github.com/zed-industries/zed)** - Source code and examples
- **[gpui.rs](https://www.gpui.rs/)** - Interactive documentation
- **[Zed Blog](https://zed.dev/blog)** - Updates and deep dives

### Community
- **[Zed Discord](https://zed.dev/community-links)** - Get help and discuss
- **[GitHub Discussions](https://github.com/zed-industries/zed/discussions)** - Q&A and ideas
- **[GitHub Issues](https://github.com/zed-industries/zed/issues)** - Report bugs

### Studying the Source
Learn from real-world GPUI usage:
- `crates/gpui/` - Framework source code
- `crates/ui/src/components/` - Reusable UI components
- `crates/editor/` - Complex editor component
- `crates/workspace/` - Application workspace

---

## Documentation Maintenance

This documentation is maintained alongside the GPUI framework. If you find errors or have suggestions:

1. Open an issue on [GitHub](https://github.com/zed-industries/zed/issues)
2. Submit a pull request with improvements
3. Ask questions in the [Zed Discord](https://zed.dev/community-links)

---

## Version Information

GPUI is pre-1.0 and under active development. APIs may change between versions. This documentation corresponds to the current main branch.

For the latest updates, check:
- [CHANGELOG](../../../CHANGELOG.md)
- [Releases](https://github.com/zed-industries/zed/releases)

---

Happy coding with GPUI! 🎉
