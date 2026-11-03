A write walks the live set for an expired claim only once the soonest `valid_to` in that workspace has passed.
It used to walk every claim on every write.
On a pack of 20,000 live claims the snapshot stage median fell from 2.60 ms with that walk to 0.00 ms with the soonest `valid_to` cached.
