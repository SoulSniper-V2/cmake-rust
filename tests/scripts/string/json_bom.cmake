string(JSON a GET "\xEF\xBB\xBF{\"a\":1}" a)
message("a=${a}")
string(JSON b GET " \t\n {\"x\":1} \t " x)
message("b=${b}")
