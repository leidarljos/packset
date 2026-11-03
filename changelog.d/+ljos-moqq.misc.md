The start test measures what one fault maps of the pack before each start, so its bound on ext4 is 2836 KiB where it was 26848 KiB.
Its start child pins glibc's trim threshold and warms up over a copy of the pack.
The warm-up stops when the test binary's resident pages stop growing, or at most one start past the binary's page count.
`measurements/start.txt` records the test's runs before and after the fix, interleaved draw by draw, with their raw logs.
