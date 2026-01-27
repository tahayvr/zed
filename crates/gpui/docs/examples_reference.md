# GPUI Examples Reference

This guide explains all the examples in the GPUI repository with descriptions, key concepts, and how to run them.

## Running Examples

All examples can be run from the Zed repository root:

```bash
cargo run -p gpui --example <example_name>
```

## Basic Examples

### hello_world.rs

**Description**: The simplest GPUI application showing basic window creation and rendering.

**Key Concepts**:
- Creating an `Application`
- Opening a window with `cx.open_window()`
- Implementing the `Render` trait
- Basic styling with Tailwind-like API
- Using `div()` elements

**What You'll Learn**:
- How to structure a minimal GPUI app
- Basic flexbox layout
- Color styling
- Text rendering

**Run**:
```bash
cargo run -p gpui --example hello_world
```

**Key Code**:
```rust
impl Render for HelloWorld {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .bg(rgb(0x505050))
            .size(px(500.0))
            .justify_center()
            .items_center()
            .child(format!("Hello, {}!", &self.text))
    }
}
```

---

### input.rs

**Description**: A complete text input implementation with selection, clipboard support, and keyboard navigation.

**Key Concepts**:
- Focus management with `FocusHandle`
- Actions for keyboard commands
- Text editing with `ElementInputHandler`
- Mouse event handling
- Clipboard integration

**What You'll Learn**:
- How to create interactive text inputs
- Handling keyboard events
- Managing text selection
- Implementing copy/paste/cut

**Run**:
```bash
cargo run -p gpui --example input
```

**Key Code**:
```rust
actions!(
    text_input,
    [Backspace, Delete, Left, Right, SelectAll, Paste, Cut, Copy]
);

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
```

---

### drag_drop.rs

**Description**: Demonstrates drag and drop functionality with visual feedback.

**Key Concepts**:
- Drag and drop API
- Dynamic styling based on state
- Creating draggable elements
- Drop targets

**What You'll Learn**:
- How to implement drag and drop
- Rendering drag previews
- Handling drop events
- Conditional styling

**Run**:
```bash
cargo run -p gpui --example drag_drop
```

**Key Code**:
```rust
div()
    .on_drag(drag_info, |info: &DragInfo, position, _, cx| {
        cx.new(|_| info.position(position))
    })
    .on_drop(cx.listener(|this, info: &DragInfo, _, _| {
        this.drop_on = Some(*info);
    }))
```

---

## Layout Examples

### grid_layout.rs

**Description**: Shows how to create grid-based layouts.

**Key Concepts**:
- CSS Grid layout
- Responsive grids
- Grid gap and alignment

**What You'll Learn**:
- Creating grid layouts
- Grid sizing and spacing

**Run**:
```bash
cargo run -p gpui --example grid_layout
```

---

### scrollable.rs

**Description**: Demonstrates scrollable containers.

**Key Concepts**:
- Scroll containers
- Overflow handling
- Scroll events

**What You'll Learn**:
- Creating scrollable views
- Managing scroll state

**Run**:
```bash
cargo run -p gpui --example scrollable
```

---

## Styling Examples

### gradient.rs

**Description**: Shows various gradient effects and color blending.

**Key Concepts**:
- Gradient backgrounds
- Color interpolation
- Multiple gradient types

**What You'll Learn**:
- Creating linear gradients
- Radial gradients
- Complex color effects

**Run**:
```bash
cargo run -p gpui --example gradient
```

---

### shadow.rs

**Description**: Comprehensive demonstration of shadow effects.

**Key Concepts**:
- Box shadows
- Multiple shadows
- Shadow blur and spread
- Shadow colors and opacity

**What You'll Learn**:
- Different shadow types
- Creating depth with shadows
- Combining multiple shadows

**Run**:
```bash
cargo run -p gpui --example shadow
```

---

### opacity.rs

**Description**: Demonstrates opacity and transparency effects.

**Key Concepts**:
- Element opacity
- Color alpha channels
- Layering transparent elements

**What You'll Learn**:
- Controlling transparency
- Creating overlay effects

**Run**:
```bash
cargo run -p gpui --example opacity
```

---

### pattern.rs

**Description**: Shows how to create repeating patterns and textures.

**Key Concepts**:
- Pattern fills
- Background patterns
- Pattern repetition

**What You'll Learn**:
- Creating visual patterns
- Texture fills

**Run**:
```bash
cargo run -p gpui --example pattern
```

---

## Animation Examples

### animation.rs

**Description**: Demonstrates various animation techniques.

**Key Concepts**:
- Animation transitions
- Easing functions
- Animated properties
- Animation timing

**What You'll Learn**:
- Creating smooth animations
- Controlling animation timing
- Animating colors, positions, sizes

**Run**:
```bash
cargo run -p gpui --example animation
```

**Key Concepts**:
```rust
// Animations happen through state changes and transitions
cx.spawn(async move |this, cx| {
    loop {
        // Update animated property
        this.update(cx, |view, cx| {
            view.animated_value = new_value;
            cx.notify();
        }).ok();
        
        // Wait for next frame
        cx.background_executor.timer(Duration::from_millis(16)).await;
    }
}).detach();
```

---

## Media Examples

### image_loading.rs

**Description**: Shows how to load and display images.

**Key Concepts**:
- Loading images from URLs
- Image caching
- Async image loading
- Placeholder handling

**What You'll Learn**:
- Displaying images in UI
- Handling image load states
- Image sizing and scaling

**Run**:
```bash
cargo run -p gpui --example image_loading
```

---

### image_gallery.rs

**Description**: A complete image gallery with thumbnails and full-view.

**Key Concepts**:
- Image grids
- Click to expand
- Multiple image sources
- Layout transitions

**What You'll Learn**:
- Building image galleries
- Managing multiple images
- Interactive image viewers

**Run**:
```bash
cargo run -p gpui --example image_gallery
```

---

### gif_viewer.rs

**Description**: Demonstrates animated GIF playback.

**Key Concepts**:
- GIF animation
- Frame-based animation
- Animation controls

**What You'll Learn**:
- Playing animated GIFs
- Controlling playback

**Run**:
```bash
cargo run -p gpui --example gif_viewer
```

---

## Advanced Examples

### data_table.rs

**Description**: A comprehensive data table implementation with sorting, filtering, and selection.

**Key Concepts**:
- Table rendering
- Row/column management
- Sorting and filtering
- Selection state
- Virtual scrolling

**What You'll Learn**:
- Building complex table UIs
- Efficient list rendering
- Data manipulation

**Run**:
```bash
cargo run -p gpui --example data_table
```

---

### testing.rs

**Description**: Demonstrates GPUI's testing infrastructure.

**Key Concepts**:
- Unit testing GPUI apps
- Testing async code
- Simulating user interactions
- Testing entity updates

**What You'll Learn**:
- Writing GPUI tests
- Testing UI interactions
- Async test patterns

**Run Tests**:
```bash
cargo test -p gpui --example testing --features test-support
```

**Run App**:
```bash
cargo run -p gpui --example testing
```

**Key Test Code**:
```rust
#[gpui::test]
fn test_counter_increment(cx: &mut TestAppContext) {
    let counter = cx.new(|_| Counter { count: 0 });
    
    counter.update(cx, |counter, cx| {
        counter.increment(&Increment, &mut Window::default(), cx);
    });
    
    assert_eq!(counter.read(cx).count, 1);
}
```

---

### uniform_list.rs

**Description**: Efficient list rendering for large datasets with uniform item heights.

**Key Concepts**:
- Virtual scrolling
- Efficient rendering of large lists
- Uniform item heights
- Performance optimization

**What You'll Learn**:
- Rendering thousands of items efficiently
- Virtual list patterns
- Performance best practices

**Run**:
```bash
cargo run -p gpui --example uniform_list
```

---

### tree.rs

**Description**: Tree view with expand/collapse functionality.

**Key Concepts**:
- Hierarchical data rendering
- Expand/collapse state
- Nested structures
- Tree navigation

**What You'll Learn**:
- Building tree views
- Managing hierarchical state
- Tree interaction patterns

**Run**:
```bash
cargo run -p gpui --example tree
```

---

## UI Components

### popover.rs

**Description**: Shows how to create popover menus and tooltips.

**Key Concepts**:
- Overlay positioning
- Popover anchoring
- Click-outside to close
- Portal rendering

**What You'll Learn**:
- Creating popovers
- Positioning floating elements
- Overlay patterns

**Run**:
```bash
cargo run -p gpui --example popover
```

---

### tab_stop.rs

**Description**: Demonstrates tab navigation and focus order.

**Key Concepts**:
- Tab order management
- Focus traversal
- Tab stops
- Keyboard navigation

**What You'll Learn**:
- Implementing tab navigation
- Managing focus order
- Keyboard accessibility

**Run**:
```bash
cargo run -p gpui --example tab_stop
```

---

### focus_visible.rs

**Description**: Shows focus indication for keyboard navigation.

**Key Concepts**:
- Focus indicators
- Keyboard vs mouse focus
- Accessibility patterns
- Visual focus states

**What You'll Learn**:
- Implementing focus indicators
- Distinguishing input methods
- Accessibility best practices

**Run**:
```bash
cargo run -p gpui --example focus_visible
```

---

## Text Examples

### text.rs

**Description**: Comprehensive text rendering and styling examples.

**Key Concepts**:
- Text rendering
- Font styles and weights
- Text sizing
- Text colors
- Line height and spacing

**What You'll Learn**:
- Text styling options
- Typography in GPUI
- Text layout

**Run**:
```bash
cargo run -p gpui --example text
```

---

### text_layout.rs

**Description**: Advanced text layout features.

**Key Concepts**:
- Multi-line text
- Text wrapping
- Text alignment
- Line breaks

**What You'll Learn**:
- Complex text layouts
- Text flow and wrapping

**Run**:
```bash
cargo run -p gpui --example text_layout
```

---

### text_wrapper.rs

**Description**: Text wrapping and overflow handling.

**Key Concepts**:
- Text wrapping strategies
- Overflow ellipsis
- Text truncation
- Word breaking

**What You'll Learn**:
- Handling long text
- Text overflow patterns

**Run**:
```bash
cargo run -p gpui --example text_wrapper
```

---

## Window Management

### window.rs

**Description**: Window creation and management examples.

**Key Concepts**:
- Multiple windows
- Window options
- Window sizing
- Window positioning

**What You'll Learn**:
- Creating multiple windows
- Configuring window properties
- Window lifecycle

**Run**:
```bash
cargo run -p gpui --example window
```

---

### window_positioning.rs

**Description**: Demonstrates window positioning strategies.

**Key Concepts**:
- Window bounds
- Screen-relative positioning
- Centered windows
- Custom positioning

**What You'll Learn**:
- Positioning windows on screen
- Multi-monitor support

**Run**:
```bash
cargo run -p gpui --example window_positioning
```

---

### window_shadow.rs

**Description**: Window-level shadow effects.

**Key Concepts**:
- Native window shadows
- Platform-specific styling
- Window decorations

**What You'll Learn**:
- Platform window styling
- Native look and feel

**Run**:
```bash
cargo run -p gpui --example window_shadow
```

---

### set_menus.rs

**Description**: Creating native menu bars.

**Key Concepts**:
- Menu bar creation
- Menu items
- Menu actions
- Platform menus

**What You'll Learn**:
- Creating application menus
- Menu item handlers
- Native menu integration

**Run**:
```bash
cargo run -p gpui --example set_menus
```

---

### on_window_close_quit.rs

**Description**: Handling window close events and app quit.

**Key Concepts**:
- Window close handling
- Application quit
- Cleanup on close
- Preventing close

**What You'll Learn**:
- Managing window lifecycle
- Application shutdown
- Save prompts before closing

**Run**:
```bash
cargo run -p gpui --example on_window_close_quit
```

---

## Graphics Examples

### painting.rs

**Description**: Low-level drawing and painting APIs.

**Key Concepts**:
- Custom drawing
- Path creation
- Shapes and curves
- Paint fills and strokes

**What You'll Learn**:
- Drawing custom graphics
- Using the painting API
- Creating complex shapes

**Run**:
```bash
cargo run -p gpui --example painting
```

---

### paths_bench.rs

**Description**: Performance benchmarking for path rendering.

**Key Concepts**:
- Path performance
- Rendering optimization
- Benchmarking techniques

**What You'll Learn**:
- Performance testing
- Optimization strategies

**Run**:
```bash
cargo run -p gpui --example paths_bench
```

---

## Platform-Specific

### layer_shell.rs (Linux only)

**Description**: Wayland layer shell integration for Linux.

**Key Concepts**:
- Layer shell protocol
- Desktop shell integration
- Panel and overlay windows

**What You'll Learn**:
- Linux-specific window types
- Wayland integration

**Run** (Linux only):
```bash
cargo run -p gpui --example layer_shell
```

---

### mouse_pressure.rs (macOS only)

**Description**: Force Touch and pressure sensitivity on macOS.

**Key Concepts**:
- Pressure-sensitive input
- Force Touch events
- Variable pressure drawing

**What You'll Learn**:
- Pressure-sensitive interactions
- macOS-specific input

**Run** (macOS only):
```bash
cargo run -p gpui --example mouse_pressure
```

---

## Example Categories Summary

| Category | Examples | Focus |
|----------|----------|-------|
| **Basics** | hello_world | Getting started |
| **Input** | input, drag_drop, tab_stop, focus_visible | User interaction |
| **Layout** | grid_layout, scrollable | Layout systems |
| **Styling** | gradient, shadow, opacity, pattern | Visual styling |
| **Animation** | animation | Motion and transitions |
| **Media** | image_loading, image_gallery, gif_viewer | Images and media |
| **Data** | data_table, uniform_list, tree | Data visualization |
| **Text** | text, text_layout, text_wrapper | Typography |
| **Windows** | window, window_positioning, window_shadow, set_menus, on_window_close_quit | Window management |
| **Graphics** | painting, paths_bench | Custom drawing |
| **Testing** | testing | Testing patterns |
| **Advanced** | popover | Complex components |

## Learning Path

### Beginner
1. **hello_world** - Start here!
2. **text** - Learn text rendering
3. **gradient** - Understand styling
4. **window** - Window basics

### Intermediate
1. **input** - Interactive components
2. **drag_drop** - Advanced interaction
3. **animation** - Add motion
4. **data_table** - Complex UIs

### Advanced
1. **testing** - Test your apps
2. **uniform_list** - Performance optimization
3. **painting** - Custom graphics
4. **popover** - Advanced patterns

## Tips for Exploring Examples

1. **Run the example first** - See it in action before reading code
2. **Read the source code** - Examples are heavily commented
3. **Modify and experiment** - Change values and see what happens
4. **Combine concepts** - Mix features from different examples
5. **Build something new** - Use examples as templates

## Additional Resources

- [Beginner's Guide](beginner_guide.md) - Complete tutorial
- [Quick Start](quick_start.md) - Get started in 5 minutes
- [Contexts](contexts.md) - Understanding contexts
- [Key Dispatch](key_dispatch.md) - Keyboard actions

Happy exploring! 🎨
