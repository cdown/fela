- Add a Windows icon
- Get rid of prefetch_{ahead,behind} and instead make them respect cache_mb.
  Then we can have prefetch_forwards_bias=N, where N is how many more mb to
  allot forwards vs backwards (eg. 3 on 512mb is 128mb backwards, 384mb
  forwards).
