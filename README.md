# fileqry

A 'salsa' file query example with (batched) cache invalidation.

## usage

1. open two terminal windows

2. execute in the first window:
   ```sh
   git clone https://github.com/zserik/fileqry.git
   cd fileqry
   RUST_LOG="warn,fileqry=trace" cargo run
   ```

3. in the second window
   ```sh
   cd fileqry
   rm test.txt
   cat >> test.txt
   ```

4. enjoy the debug output of `fileqry` in the first window while you append file
   contents to the file `test.txt` in the second window.
