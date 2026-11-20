file(WRITE input/child/value content)
file(INSTALL DESTINATION prefix TYPE FILE FILES child/value FILES_FROM_DIR input)
file(READ prefix/child/value out)
message("[${out}]")
