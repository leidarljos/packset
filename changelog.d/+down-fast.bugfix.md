`packset stop` with nothing listening returns at once and does not start a writer.
`packset search` with nothing listening returns at once and names `packset ensure`, the same sentence `status` uses.
A refused connection on those two commands is not retried for the whole client budget.
