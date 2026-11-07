//! The clipboard's image for a paste the panel's web view could not read. WebKitGTK gives a
//! paste no data when the clipboard holds only an image (a Flameshot capture, for one), so the
//! composer asks the app, which reads it through GTK. The other systems' web views paste images
//! themselves, so there it answers that it has none.

use base64::Engine;

/// Larger images are not sent to the panel: the bytes travel base64 encoded over IPC.
const MAX_PNG_BYTES: usize = 32 * 1024 * 1024;

/// The clipboard's image as base64 PNG, or `None` when it holds none (always `None` off Linux).
#[tauri::command]
pub async fn clipboard_image(app: tauri::AppHandle) -> Result<Option<String>, String> {
    #[cfg(target_os = "linux")]
    return linux::read_png(&app).map(|png| png.as_deref().and_then(encode_png));
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Ok(None)
    }
}

/// Base64 of a PNG small enough to send to the panel.
fn encode_png(png: &[u8]) -> Option<String> {
    (!png.is_empty() && png.len() <= MAX_PNG_BYTES).then(|| base64::engine::general_purpose::STANDARD.encode(png))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::mpsc;
    use std::time::Duration;

    /// A clipboard owner that does not answer (a closed or stuck app) gives no image.
    const WAIT: Duration = Duration::from_secs(3);

    /// Reads the clipboard's image on the main thread, where GTK lives, and encodes it as PNG.
    pub fn read_png(app: &tauri::AppHandle) -> Result<Option<Vec<u8>>, String> {
        let (sender, receiver) = mpsc::channel();
        app.run_on_main_thread(move || {
            let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
            clipboard.request_image(move |_, image| {
                let png = image.and_then(|pixbuf| pixbuf.save_to_bufferv("png", &[]).ok());
                let _ = sender.send(png);
            });
        })
        .map_err(|error| error.to_string())?;
        Ok(receiver.recv_timeout(WAIT).ok().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_only_an_image_small_enough_for_the_panel() {
        assert_eq!(encode_png(&[0x89, b'P', b'N', b'G']).as_deref(), Some("iVBORw=="));
        assert_eq!(encode_png(&[]), None);
        assert_eq!(encode_png(&vec![0; MAX_PNG_BYTES + 1]), None);
        assert!(encode_png(&vec![0; MAX_PNG_BYTES]).is_some());
    }
}
