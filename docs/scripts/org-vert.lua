-- Org writes a bar inside a table cell as \vert. Outside verbatim pandoc's
-- Org reader turns that into |, but inside =verbatim= or ~code~ Org keeps
-- the text as typed, so the page showed a literal \vert. Give code spans the
-- bar the entity names. The sources write deny\vert ask for deny|ask,
-- with the space only ending the entity name, and ID \vert - for ID | -,
-- so a space after the entity is dropped unless one comes before it too.
function Code(el)
  el.text = el.text
    :gsub("(%S)\\vert ", "%1|")
    :gsub("\\vert{}", "|")
    :gsub("\\vert", "|")
  return el
end
