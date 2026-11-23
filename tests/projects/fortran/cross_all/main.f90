program fmain
  use fstat_mod
  use fmore_mod
  implicit none
  if (stat_answer() /= 42) stop 1
  if (scaled() /= 84) stop 2
end program fmain
