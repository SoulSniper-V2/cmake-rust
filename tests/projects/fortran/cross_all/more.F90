module fmore_mod
  use fstat_mod
contains
  function scaled() result(r)
    integer :: r
#ifdef EXTRA
    r = stat_answer() * EXTRA
#else
    r = stat_answer()
#endif
  end function scaled
end module fmore_mod
