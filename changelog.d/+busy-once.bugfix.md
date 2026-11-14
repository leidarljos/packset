A memory-pressure refusal says `packsetd is busy` once, then what to do next.
A read or paused maintenance says to wait and run the command again. A large write says to wait or send a smaller one. A full append log says to wait for it to drain, then run the command again.
