import sys
import re

files = [
    'crates/holmes-runtime/src/runtime.rs',
    'crates/holmes-runtime/src/action.rs',
    'crates/holmes-runtime/src/dialogue.rs'
]

for filename in files:
    with open(filename, 'r') as f:
        content = f.read()
    
    # fix sink.events
    content = content.replace('sink.events', 'sink.yields()')
    content = content.replace('sink.yields().first()', 'sink.yields().first().unwrap()') # though yields() returns Vec so first() returns Option<&RuntimeYield>.
    content = content.replace('sink.yields()()', 'sink.yields()')
    
    # fix missing error/usage in tests
    content = re.sub(r'RuntimeYield::ToolFinished\s*\{([^}]*?)success([^}]*?)\}', 
                     lambda m: m.group(0) if 'error:' in m.group(0) or '..' in m.group(0) else m.group(0).replace('}', ', error: None, usage: None }'), content)
                     
    content = re.sub(r'RuntimeYield::FinalAnswer\s*\{([^}]*?)content([^}]*?)\}', 
                     lambda m: m.group(0) if 'usage:' in m.group(0) or '..' in m.group(0) else m.group(0).replace('}', ', usage: None }'), content)
                     
    content = content.replace(', .. , ..', ', ..')
    content = content.replace('success: false, .. }', 'success: false, .. }')

    with open(filename, 'w') as f:
        f.write(content)

