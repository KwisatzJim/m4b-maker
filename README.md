# m4b-maker
a Rust app to take mp3's and make an Audiobook in .m4b format.  

### To run it:

install rust

(official method from rust-lang.org)
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Clone this repository and run the app:

```
git clone https://github.com/KwisatzJim/m4b-maker
```

```
cd m4b-maker
```

```
cargo run
```

### To use it:

Use “Dark mode” to switch the appearance.

Click “Select files…” and choose “Individual audio files…” or “Audio folder…”.
Folder selection loads MP3, M4A, and WAV files directly inside the folder; subfolders are not included. Files are automatically sorted by filename with numbers in numeric order (1, 2, 10).
Expand the selected file list to check export order and use Up/Down to adjust it.
“Sort by chapter number” restores the automatic order.
Each input file becomes a chapter marker, following that export order. Chapter names
use the track title stored inside the file, or the filename when no title is stored.
The app uses FFprobe to read durations before exporting; unreadable durations stop
the export with an explanation in Conversion details.

Title and Author are filled automatically when empty: the app checks audio tags
(using `ffprobe`, included with FFmpeg), then filenames in `Author - Title` format.
Album is preferred for the book title because track titles often name chapters.
Review or edit the detected values before exporting. Click “New book” before
selecting another book to clear the previous files, metadata, artwork, and status.
Your input/output folders and theme stay unchanged. “New book” is disabled during export.

Embedded cover artwork is detected automatically and shown in a cover preview. Use “Choose cover…” to supply a JPEG or PNG, or “Remove cover” to export without artwork.

Click “Export audiobook…” and choose a destination. The suggested filename uses the book title.
Input and output folders are remembered separately, including after restarting the app.

click Save

Use “Cancel conversion” to stop chapter preparation or encoding. Unfinished output
is discarded; an existing destination is replaced only after a successful export.

Conversion status updates in place, including estimated encoding time remaining
based on audio duration and current conversion speed. The estimate can change as
speed changes; final file preparation can take additional time. Expand “Conversion details” for recent warnings and errors.
