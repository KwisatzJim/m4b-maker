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

click on "Toggle Theme" to... toggle the theme [between light and dark]

click on "Select Files" and choose your .mp3's

Fill in the Title and Author of the audiobook

click on Export to .m4b and choose destination and give the output file a name.

click Save

the output of ffmpeg will appear in the window and update in realtime.
