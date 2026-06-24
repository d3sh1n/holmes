import sys

files = [
    'crates/holmes-runtime/src/runtime.rs',
    'crates/holmes-runtime/src/action.rs'
]

for filename in files:
    with open(filename, 'r') as f:
        content = f.read()

    # fix double comma before error
    content = content.replace(",\n                , error: None, usage: None }", ",\n                 error: None, usage: None }")
    content = content.replace(",\n            , error: None, usage: None }", ",\n             error: None, usage: None }")
    content = content.replace(", error: None, usage: None }", " error: None, usage: None }") # remove leading comma if there's an issue? No, wait. 

    # Better logic for double commas:
    content = content.replace(", ,", ",")
    content = content.replace(",\n                error: None, usage: None }", ",\n                error: None, usage: None }")

    # fix .unwrap()
    content = content.replace("sink.yields().first().unwrap()", "sink.yields().first().map(|y| y.clone())")
    content = content.replace("sink.yields().last().unwrap()", "sink.yields().last().map(|y| y.clone())")
    content = content.replace("sink.yields().first()", "sink.yields().first().map(|y| y.clone())")
    content = content.replace("sink.yields().last()", "sink.yields().last().map(|y| y.clone())")
    
    # avoid map(|y| y.clone()).map(|y| y.clone())
    content = content.replace(".map(|y| y.clone()).map(|y| y.clone())", ".map(|y| y.clone())")

    # matches!(event,
    content = content.replace("matches!(event, RuntimeYield", "matches!(event.data, RuntimeYield")

    with open(filename, 'w') as f:
        f.write(content)

