file(WRITE input "abc\ndef\n")
file(STRINGS input output REGEX "[")
