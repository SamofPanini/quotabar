use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Position};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub fn resized_popover_frame(
    current: LogicalRect,
    requested_width: f64,
    requested_height: f64,
    display: LogicalRect,
) -> Result<LogicalRect, String> {
    if !requested_width.is_finite()
        || !requested_height.is_finite()
        || requested_width <= 0.0
        || requested_height <= 0.0
    {
        return Err("window size must be finite and positive".into());
    }
    if !current.x.is_finite()
        || !current.y.is_finite()
        || !current.width.is_finite()
        || !current.height.is_finite()
        || !display.x.is_finite()
        || !display.y.is_finite()
        || !display.width.is_finite()
        || !display.height.is_finite()
        || display.width <= 0.0
        || display.height <= 0.0
    {
        return Err("window and display rectangles must be finite and positive".into());
    }

    let width = requested_width;
    let height = requested_height.min((display.height - 8.0).max(0.0));
    if height <= 0.0 {
        return Err("display is too short for popover".into());
    }
    let centered_x = current.x + current.width / 2.0 - width / 2.0;
    let max_x = (display.x + display.width - width).max(display.x);
    let max_y = (display.y + display.height - height).max(display.y);
    Ok(LogicalRect {
        x: centered_x.clamp(display.x, max_x),
        y: current.y.clamp(display.y, max_y),
        width,
        height,
    })
}

pub async fn resize_window(app: AppHandle, height: f64, width: f64) -> Result<(), String> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err("window size must be finite and positive".into());
    }
    if let Some(window) = app.get_webview_window("main") {
        let frame = (|| {
            let monitor = window
                .current_monitor()
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "current monitor is unavailable".to_string())?;
            let scale = monitor.scale_factor();
            let position = window.outer_position().map_err(|e| e.to_string())?;
            let outer_size = window.outer_size().map_err(|e| e.to_string())?;
            let monitor_position = monitor.position();
            let monitor_size = monitor.size();
            resized_popover_frame(
                LogicalRect {
                    x: position.x as f64 / scale,
                    y: position.y as f64 / scale,
                    width: outer_size.width as f64 / scale,
                    height: outer_size.height as f64 / scale,
                },
                width,
                height,
                LogicalRect {
                    x: monitor_position.x as f64 / scale,
                    y: monitor_position.y as f64 / scale,
                    width: monitor_size.width as f64 / scale,
                    height: monitor_size.height as f64 / scale,
                },
            )
        })();
        if let Ok(frame) = frame {
            window
                .set_size(LogicalSize::new(frame.width, frame.height))
                .map_err(|e| e.to_string())?;
            window
                .set_position(Position::Logical(LogicalPosition::new(frame.x, frame.y)))
                .map_err(|e| e.to_string())?;
        } else {
            window
                .set_size(LogicalSize::new(width, height))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub async fn set_dock_visibility(app: AppHandle, visible: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        if visible {
            app.set_activation_policy(ActivationPolicy::Regular)
                .map_err(|e| e.to_string())?;
        } else {
            app.set_activation_policy(ActivationPolicy::Accessory)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resized_popover_frame, LogicalRect};

    const DISPLAY: LogicalRect = LogicalRect {
        x: 0.0,
        y: 0.0,
        width: 1000.0,
        height: 800.0,
    };

    #[test]
    fn keeps_center_and_top_for_each_supported_width() {
        let current = LogicalRect {
            x: 330.0,
            y: 40.0,
            width: 340.0,
            height: 300.0,
        };
        for width in [340.0, 425.0, 510.0] {
            let frame = resized_popover_frame(current, width, 400.0, DISPLAY).expect("valid frame");
            assert_eq!(frame.x + frame.width / 2.0, current.x + current.width / 2.0);
            assert_eq!(frame.y, current.y);
        }
    }

    #[test]
    fn clamps_a_frame_near_the_right_edge() {
        let current = LogicalRect {
            x: 800.0,
            y: 20.0,
            width: 340.0,
            height: 300.0,
        };
        let frame = resized_popover_frame(current, 510.0, 300.0, DISPLAY).expect("valid frame");
        assert_eq!(frame.x, 490.0);
        assert!(frame.x + frame.width <= DISPLAY.x + DISPLAY.width);
    }

    #[test]
    fn limits_height_to_display_margin() {
        let current = LogicalRect {
            x: 20.0,
            y: 10.0,
            width: 340.0,
            height: 300.0,
        };
        let frame = resized_popover_frame(current, 340.0, 900.0, DISPLAY).expect("valid frame");
        assert_eq!(frame.height, 792.0);
    }

    #[test]
    fn rejects_invalid_sizes() {
        let current = LogicalRect {
            x: 20.0,
            y: 10.0,
            width: 340.0,
            height: 300.0,
        };
        for (width, height) in [
            (0.0, 100.0),
            (100.0, 0.0),
            (f64::NAN, 100.0),
            (100.0, f64::INFINITY),
        ] {
            assert!(resized_popover_frame(current, width, height, DISPLAY).is_err());
        }
    }
}
