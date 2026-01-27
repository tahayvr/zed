# GPUI Beginner's Guide

Welcome to GPUI! This comprehensive guide will help you get started with building GUI applications in Rust using GPUI, the custom UI framework that powers Zed.

## Table of Contents

1. [What is GPUI?](#what-is-gpui)
2. [Installation and Setup](#installation-and-setup)
3. [Your First GPUI Application](#your-first-gpui-application)
4. [Core Concepts](#core-concepts)
5. [Building User Interfaces](#building-user-interfaces)
6. [Styling Elements](#styling-elements)
7. [State Management](#state-management)
8. [Event Handling](#event-handling)
9. [Interactive Components](#interactive-components)
10. [Async Operations](#async-operations)
11. [Testing](#testing)
12. [Best Practices](#best-practices)
13. [Next Steps](#next-steps)

## What is GPUI?

GPUI is a **hybrid immediate and retained mode, GPU-accelerated UI framework** for Rust. It was created to build the Zed code editor and provides a powerful, performant way to create desktop applications.

### Key Features

- **GPU-Accelerated**: Renders using Metal (macOS), DirectX (Windows), or Vulkan (Linux)
- **Tailwind-like Styling**: Familiar CSS-inspired API for styling components
- **Type-Safe**: Leverages Rust's type system for compile-time safety
- **Reactive State Management**: Built-in state management with entities and contexts
- **Async Support**: Integrated async executor for background tasks
- **Testing Framework**: Built-in testing utilities for UI testing

## Installation and Setup

### Prerequisites

**macOS:**
```bash
# Install Xcode from the App Store or Apple Developer website
# Then install command line tools:
xcode-select --install
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
```

**Linux:**
```bash
# Install required dependencies (Ubuntu/Debian):
sudo apt install libssl-dev pkg-config libfontconfig-dev
```

**Windows:**
```bash
# Install Visual Studio with C++ build tools
```

### Adding GPUI to Your Project

Add GPUI to your `Cargo.toml`:

```toml
[dependencies]
gpui = { git = "https://github.com/zed-industries/zed" }
```

Or if using a specific version:

```toml
[dependencies]
gpui = "*"
```

### Running Your First Example

Clone the Zed repository to explore examples:

```bash
git clone https://github.com/zed-industries/zed
cd zed
cargo run -p gpui --example hello_world
```

## Your First GPUI Application

Let's create a simple "Hello World" application:

```rust
use gpui::{
    App, Application, Bounds, Context, SharedString, Window, 
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

// Define your application state
struct HelloWorld {
    text: SharedString,
}

// Implement the Render trait to define the UI
impl Render for HelloWorld {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .bg(rgb(0x2e3440))
            .size_full()
            .justify_center()
            .items_center()
            .text_xl()
            .text_color(rgb(0xeceff4))
            .child(format!("Hello, {}!", &self.text))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        // Create a window with bounds
        let bounds = Bounds::centered(None, size(px(400.), px(300.)), cx);
        
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                // Create the root view
                cx.new(|_| HelloWorld {
                    text: "GPUI".into(),
                })
            },
        )
        .unwrap();
        
        cx.activate(true);
    });
}
```

### Breaking It Down

1. **Application**: Entry point for your app created with `Application::new()`
2. **Window**: Created using `cx.open_window()` with specified options
3. **Entity (View)**: Created with `cx.new()` - this is your root UI component
4. **Render Trait**: Defines how to render your UI using elements

## Core Concepts

### 1. Application

The `Application` is the root of every GPUI app. It manages the event loop and windows.

```rust
Application::new().run(|cx: &mut App| {
    // Initialize your app here
});
```

### 2. Contexts

GPUI uses context parameters (typically named `cx`) to provide access to application state and services.

#### `App`
The root context for global state access. Used to read/update entities.

```rust
Application::new().run(|cx: &mut App| {
    // cx is an &mut App
});
```

#### `Context<T>`
Provided when updating an `Entity<T>`. It dereferences to `App`, so you can use it anywhere `App` is accepted.

```rust
impl Render for MyView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // cx is a &mut Context<MyView>
        // You can call cx.notify(), cx.emit(), etc.
    }
}
```

#### `AsyncApp` and `AsyncWindowContext`
Created with `cx.to_async()` for use across `await` points in async code.

```rust
cx.spawn(async move |cx| {
    // cx is an AsyncApp here
    // Can be held across await points
})
```

#### `Window`
Provides access to window state (focus, actions, drawing). Passed as `window` parameter.

```rust
impl Render for MyView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // window provides window-specific operations
    }
}
```

### 3. Entities

An `Entity<T>` is a smart pointer to state managed by GPUI. Think of it like an `Rc` but managed by the framework.

```rust
// Create an entity
let counter = cx.new(|_| Counter { count: 0 });

// Read the entity
counter.read(cx); // Returns &Counter

// Update the entity
counter.update(cx, |counter, cx| {
    counter.count += 1;
    cx.notify(); // Notify observers of changes
});
```

When `T` implements `Render`, the `Entity<T>` is called a **view**.

### 4. Elements

Elements are the building blocks of UI. The `div()` element is the most common:

```rust
div()
    .flex()
    .gap_4()
    .child("First child")
    .child("Second child")
```

## Building User Interfaces

### The `div` Element

The `div()` element is your swiss-army knife for building UIs:

```rust
div()
    // Layout
    .flex()
    .flex_col()
    .gap_3()
    .justify_center()
    .items_center()
    
    // Sizing
    .size_full()      // width and height 100%
    .w(px(300.))      // width in pixels
    .h(px(200.))      // height in pixels
    
    // Styling
    .bg(rgb(0x2e3440))
    .border_1()
    .rounded_md()
    
    // Children
    .child("Hello")
    .child(div().child("Nested"))
```

### Composition

Build reusable components using the `Render` trait:

```rust
struct Card {
    title: SharedString,
    content: SharedString,
}

impl Render for Card {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .bg(rgb(0x3b4252))
            .rounded_lg()
            .shadow_md()
            .child(
                div()
                    .text_xl()
                    .font_semibold()
                    .text_color(rgb(0xeceff4))
                    .child(self.title.clone())
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xd8dee9))
                    .child(self.content.clone())
            )
    }
}
```

### Conditional Rendering

Use `.when()` for conditional elements:

```rust
div()
    .when(show_border, |this| this.border_1())
    .when_some(optional_value, |this, value| {
        this.child(format!("Value: {}", value))
    })
```

### Lists and Children

Render multiple children with `.children()`:

```rust
let items = vec!["Apple", "Banana", "Cherry"];

div()
    .flex()
    .flex_col()
    .gap_2()
    .children(items.iter().map(|item| {
        div()
            .p_2()
            .bg(rgb(0x3b4252))
            .child(*item)
    }))
```

## Styling Elements

GPUI uses a Tailwind-like API for styling. Here's a comprehensive overview:

### Layout

```rust
div()
    // Flexbox
    .flex()              // display: flex
    .flex_col()          // flex-direction: column
    .flex_row()          // flex-direction: row
    .flex_wrap()         // flex-wrap: wrap
    
    // Alignment
    .justify_center()    // justify-content: center
    .justify_between()   // justify-content: space-between
    .justify_start()     // justify-content: flex-start
    .justify_end()       // justify-content: flex-end
    
    .items_center()      // align-items: center
    .items_start()       // align-items: flex-start
    .items_end()         // align-items: flex-end
    
    // Gaps
    .gap_1()             // gap: 0.25rem
    .gap_2()             // gap: 0.5rem
    .gap_4()             // gap: 1rem
```

### Sizing

```rust
div()
    // Width
    .w_full()            // width: 100%
    .w(px(200.))         // width: 200px
    .w_auto()            // width: auto
    
    // Height
    .h_full()            // height: 100%
    .h(px(100.))         // height: 100px
    
    // Both
    .size_full()         // width & height: 100%
    .size(px(50.))       // width & height: 50px
    .size_8()            // width & height: 2rem
```

### Spacing

```rust
div()
    // Padding
    .p_4()               // padding: 1rem (all sides)
    .px_4()              // padding-left & right: 1rem
    .py_4()              // padding-top & bottom: 1rem
    .pt_2()              // padding-top: 0.5rem
    .pr_2()              // padding-right: 0.5rem
    .pb_2()              // padding-bottom: 0.5rem
    .pl_2()              // padding-left: 0.5rem
    
    // Margin
    .m_4()               // margin: 1rem
    .mx_4()              // margin-left & right: 1rem
    .my_4()              // margin-top & bottom: 1rem
    .mt_2()              // margin-top: 0.5rem
```

### Colors

```rust
use gpui::{rgb, rgba, hsla};

div()
    // Background
    .bg(rgb(0x2e3440))
    .bg(rgba(0x2e3440ff))
    .bg(hsla(220.0 / 360.0, 0.16, 0.22, 1.0))
    
    // Text color
    .text_color(rgb(0xeceff4))
    
    // Border color
    .border_color(rgb(0x4c566a))
```

GPUI also provides color helpers:
```rust
use gpui::{red, green, blue, yellow, black, white};

div().bg(red())
div().bg(green())
div().bg(blue())
```

### Borders

```rust
div()
    .border_1()          // border: 1px
    .border_2()          // border: 2px
    .border_t_1()        // border-top: 1px
    .border_color(rgb(0x4c566a))
    .rounded_md()        // border-radius: 0.375rem
    .rounded_lg()        // border-radius: 0.5rem
    .rounded_full()      // border-radius: 9999px
```

### Typography

```rust
div()
    .text_xs()           // font-size: 0.75rem
    .text_sm()           // font-size: 0.875rem
    .text_base()         // font-size: 1rem
    .text_lg()           // font-size: 1.125rem
    .text_xl()           // font-size: 1.25rem
    .text_2xl()          // font-size: 1.5rem
    .text_3xl()          // font-size: 1.875rem
    
    .font_bold()         // font-weight: bold
    .font_semibold()     // font-weight: 600
```

### Shadows

```rust
div()
    .shadow_sm()         // box-shadow: small
    .shadow_md()         // box-shadow: medium
    .shadow_lg()         // box-shadow: large
```

### Cursor

```rust
div()
    .cursor_pointer()
    .cursor_move()
    .cursor_text()
```

### Hover States

```rust
div()
    .hover(|this| {
        this.bg(rgb(0x434c5e))
            .shadow_lg()
    })
```

## State Management

### Reactive State with Entities

Entities are the primary way to manage state in GPUI:

```rust
struct Counter {
    count: i32,
}

impl Counter {
    fn increment(&mut self, cx: &mut Context<Self>) {
        self.count += 1;
        cx.notify(); // Notify observers that state changed
    }
}

impl Render for Counter {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(format!("Count: {}", self.count))
    }
}
```

### Notifying Changes

When state changes, call `cx.notify()` to trigger a re-render:

```rust
fn update_state(&mut self, cx: &mut Context<Self>) {
    self.value = "new value".into();
    cx.notify(); // This will cause render() to be called again
}
```

### Observing Entities

One entity can observe another:

```rust
struct Observer;

impl Observer {
    fn new(counter: Entity<Counter>, cx: &mut Context<Self>) -> Self {
        cx.observe(&counter, |this, counter, cx| {
            // Called when counter.notify() is called
            println!("Counter changed: {}", counter.read(cx).count);
        }).detach();
        
        Self
    }
}
```

### Subscriptions

Subscribe to events emitted by entities:

```rust
struct MyEvent(String);

impl EventEmitter<MyEvent> for MyEntity {}

impl Observer {
    fn new(entity: Entity<MyEntity>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&entity, |this, entity, event: &MyEvent, cx| {
            println!("Received event: {}", event.0);
        }).detach();
        
        Self
    }
}

// Emit an event
cx.emit(MyEvent("Hello".to_string()));
```

## Event Handling

### Click Events

```rust
div()
    .on_click(|event, window, cx| {
        println!("Clicked!");
    })
```

### Mouse Events

```rust
div()
    .on_mouse_down(MouseButton::Left, |event, window, cx| {
        println!("Mouse down at: {:?}", event.position);
    })
    .on_mouse_up(MouseButton::Left, |event, window, cx| {
        println!("Mouse up!");
    })
    .on_mouse_move(|event, window, cx| {
        println!("Mouse move: {:?}", event.position);
    })
```

### Using Context Listeners

When you need to update the current entity in an event handler, use `cx.listener`:

```rust
impl Render for Counter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .on_click(cx.listener(|this: &mut Counter, event, window, cx| {
                this.count += 1;
                cx.notify();
            }))
            .child(format!("Count: {}", self.count))
    }
}
```

### Drag and Drop

```rust
#[derive(Clone)]
struct DragData {
    value: String,
}

impl Render for DragData {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(&self.value)
    }
}

div()
    .on_drag(DragData { value: "Draggable".into() }, |data, position, window, cx| {
        cx.new(|_| data.clone())
    })
    .on_drop(cx.listener(|this, data: &DragData, window, cx| {
        println!("Dropped: {}", data.value);
    }))
```

## Interactive Components

### Focus Management

```rust
use gpui::{FocusHandle, Focusable};

struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextInput {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .on_click(cx.listener(|this, event, window, cx| {
                this.focus_handle.focus(window);
            }))
            .child(&self.content)
    }
}
```

### Actions

Actions are keyboard-triggered commands that can be bound to keys:

```rust
use gpui::actions;

// Define actions
actions!(text_input, [Cut, Copy, Paste, SelectAll]);

impl TextInput {
    fn copy(&mut self, _: &Copy, _window: &mut Window, _cx: &mut Context<Self>) {
        // Handle copy
    }
    
    fn paste(&mut self, _: &Paste, _window: &mut Window, _cx: &mut Context<Self>) {
        // Handle paste
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .child(&self.content)
    }
}
```

To bind keys to actions, create a keymap file (JSON):

```json
{
  "context": "TextInput",
  "bindings": {
    "cmd-c": "text_input::Copy",
    "cmd-v": "text_input::Paste"
  }
}
```

### Complex Actions with Data

```rust
#[derive(Clone, Debug, PartialEq)]
struct Move {
    direction: Direction,
    select: bool,
}

// Implement the Action trait
impl gpui::Action for Move {
    fn name(&self) -> &str {
        "Move"
    }
    
    fn debug_name() -> &'static str
    where
        Self: Sized,
    {
        "Move"
    }
}
```

## Async Operations

### Spawning Background Tasks

```rust
impl MyView {
    fn load_data(&mut self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            // Simulate async work
            let data = fetch_data().await;
            
            // Update the entity with the result
            this.update(cx, |view, cx| {
                view.data = data;
                cx.notify();
            }).ok();
        })
    }
}
```

### Background Executor

For CPU-intensive work, use the background executor:

```rust
fn process_data(&mut self, cx: &mut Context<Self>) {
    let task = cx.background_spawn(async move {
        // This runs on a background thread
        expensive_computation()
    });
    
    cx.spawn(async move |this, cx| {
        let result = task.await;
        this.update(cx, |view, cx| {
            view.result = result;
            cx.notify();
        }).ok();
    }).detach();
}
```

### Detaching Tasks

Tasks are cancelled when dropped. To keep them running, use `.detach()`:

```rust
cx.spawn(async move |this, cx| {
    // This task will run to completion
    // even if nothing holds the Task handle
}).detach();
```

## Testing

### Writing Tests

GPUI provides a testing framework with the `#[gpui::test]` macro:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    
    #[gpui::test]
    fn test_counter_increment(cx: &mut TestAppContext) {
        let counter = cx.new(|_| Counter { count: 0 });
        
        counter.update(cx, |counter, cx| {
            counter.increment(cx);
        });
        
        assert_eq!(counter.read(cx).count, 1);
    }
}
```

### Testing Async Code

```rust
#[gpui::test]
async fn test_async_loading(cx: &mut TestAppContext) {
    let view = cx.new(|_| MyView::new());
    
    let task = view.update(cx, |view, cx| view.load_data(cx));
    task.await;
    
    assert!(view.read(cx).data.is_some());
}
```

### Visual Testing

```rust
#[gpui::test]
fn test_rendering(cx: &mut TestAppContext) {
    let view = cx.new(|_| MyView::new());
    
    let window = cx.add_window(|cx| view);
    
    // Simulate user interactions
    window.update(cx, |view, window, cx| {
        // Test interactions
    });
}
```

### Running Tests

```bash
# Run all tests
cargo test

# Run tests for a specific example
cargo test -p gpui --example testing --features test-support

# Run with output
cargo test -- --nocapture
```

## Best Practices

### 1. Use Shared Strings

Use `SharedString` instead of `String` to avoid unnecessary allocations:

```rust
struct MyView {
    text: SharedString,  // Good
    // text: String,     // Avoid
}

let text: SharedString = "Hello".into();
```

### 2. Notify on State Changes

Always call `cx.notify()` when state changes to trigger re-renders:

```rust
fn update_value(&mut self, new_value: String, cx: &mut Context<Self>) {
    self.value = new_value.into();
    cx.notify(); // Don't forget this!
}
```

### 3. Use Weak Entities to Avoid Leaks

When entities reference each other, use `WeakEntity` to avoid reference cycles:

```rust
struct Parent {
    child: WeakEntity<Child>, // Use WeakEntity, not Entity
}
```

### 4. Avoid Unwrap in Event Handlers

Use `?` or `.ok()` instead of `.unwrap()` to handle errors gracefully:

```rust
cx.spawn(async move |this, cx| {
    this.update(cx, |view, cx| {
        view.count += 1;
        cx.notify();
    }).ok(); // Don't panic if the entity was dropped
})
```

### 5. Component Composition

Break complex UIs into smaller, reusable components:

```rust
// Instead of one large render method
impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .child(self.render_header(cx))
            .child(self.render_content(cx))
            .child(self.render_footer(cx))
    }
}

impl App {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().child("Header")
    }
    
    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().child("Content")
    }
    
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().child("Footer")
    }
}
```

### 6. Use Element IDs for Interactivity

When elements need to maintain state across renders, give them IDs:

```rust
div()
    .id("unique-id")
    .on_click(|event, window, cx| {
        // Handle click
    })
```

### 7. Manage Subscriptions

Store subscriptions in a field to keep them alive:

```rust
struct MyView {
    _subscriptions: Vec<Subscription>,
}

impl MyView {
    fn new(other: Entity<Other>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&other, |this, other, cx| {
            // Observer callback
        });
        
        Self {
            _subscriptions: vec![subscription],
        }
    }
}
```

### 8. Use Appropriate Context Types

- Use `cx.listener()` when the handler needs to update `self`
- Use regular closures when the handler doesn't need `self`
- Use `cx.spawn()` for async operations

## Next Steps

### Explore Examples

The GPUI repository contains many examples demonstrating various features:

```bash
# Navigate to the zed repository
cd path/to/zed

# Run examples
cargo run -p gpui --example hello_world
cargo run -p gpui --example input
cargo run -p gpui --example drag_drop
cargo run -p gpui --example animation
cargo run -p gpui --example data_table
```

### Study the Zed Codebase

The best way to learn GPUI is to study how it's used in Zed:

- **UI Components**: `crates/ui/src/components/` - Reusable UI components
- **Editor**: `crates/editor/src/` - Complex text editing component
- **Workspace**: `crates/workspace/src/` - Application workspace structure

### Read the Documentation

- [Contexts](contexts.md) - Deep dive into context types
- [Key Dispatch](key_dispatch.md) - Keyboard action system
- [GPUI README](../README.md) - Overview and architecture

### Join the Community

- [Zed Discord](https://zed.dev/community-links) - Ask questions and get help
- [Zed Blog](https://zed.dev/blog) - Learn about GPUI updates and best practices
- [GitHub Discussions](https://github.com/zed-industries/zed/discussions) - Discuss features and issues

## Example: Complete Todo App

Here's a complete example putting everything together:

```rust
use gpui::{
    App, Application, Bounds, Context, FocusHandle, Focusable, SharedString, 
    Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

actions!(todo, [AddTodo, RemoveTodo, ToggleTodo]);

#[derive(Clone)]
struct Todo {
    id: usize,
    text: SharedString,
    completed: bool,
}

struct TodoApp {
    todos: Vec<Todo>,
    next_id: usize,
    input: SharedString,
    focus_handle: FocusHandle,
}

impl TodoApp {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            todos: Vec::new(),
            next_id: 0,
            input: "".into(),
            focus_handle: cx.focus_handle(),
        }
    }
    
    fn add_todo(&mut self, _: &AddTodo, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.input.is_empty() {
            self.todos.push(Todo {
                id: self.next_id,
                text: self.input.clone(),
                completed: false,
            });
            self.next_id += 1;
            self.input = "".into();
            cx.notify();
        }
    }
    
    fn toggle_todo(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(todo) = self.todos.iter_mut().find(|t| t.id == id) {
            todo.completed = !todo.completed;
            cx.notify();
        }
    }
    
    fn remove_todo(&mut self, id: usize, cx: &mut Context<Self>) {
        self.todos.retain(|t| t.id != id);
        cx.notify();
    }
}

impl Focusable for TodoApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TodoApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .p_8()
            .gap_4()
            .track_focus(&self.focus_handle)
            .key_context("TodoApp")
            .on_action(cx.listener(Self::add_todo))
            .child(
                div()
                    .text_2xl()
                    .font_bold()
                    .text_color(rgb(0xcdd6f4))
                    .child("Todo App")
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .p_2()
                            .bg(rgb(0x313244))
                            .rounded_md()
                            .text_color(rgb(0xcdd6f4))
                            .child(&self.input)
                    )
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .bg(rgb(0x89b4fa))
                            .rounded_md()
                            .text_color(rgb(0x1e1e2e))
                            .cursor_pointer()
                            .hover(|this| this.bg(rgb(0x74c7ec)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_todo(&AddTodo, window, cx);
                            }))
                            .child("Add")
                    )
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(self.todos.iter().map(|todo| {
                        let id = todo.id;
                        div()
                            .flex()
                            .gap_2()
                            .p_2()
                            .bg(rgb(0x313244))
                            .rounded_md()
                            .child(
                                div()
                                    .w_5()
                                    .h_5()
                                    .border_2()
                                    .border_color(rgb(0x89b4fa))
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .when(todo.completed, |this| {
                                        this.bg(rgb(0x89b4fa))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_todo(id, cx);
                                    }))
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(rgb(0xcdd6f4))
                                    .when(todo.completed, |this| {
                                        this.text_color(rgb(0x6c7086))
                                    })
                                    .child(todo.text.clone())
                            )
                            .child(
                                div()
                                    .px_2()
                                    .text_color(rgb(0xf38ba8))
                                    .cursor_pointer()
                                    .hover(|this| this.text_color(rgb(0xeba0ac)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_todo(id, cx);
                                    }))
                                    .child("×")
                            )
                    }))
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(600.), px(500.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| TodoApp::new(cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
```

This guide covers the essentials of building GUI applications with GPUI. Happy coding!
