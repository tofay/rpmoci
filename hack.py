import pdb
import dnf

base = dnf.Base()
base.fill_sack()
query = dnf.query.Query(base.sack)
installed = query.installed()

from collections import defaultdict
graph = defaultdict(set)

for pkg in installed:
    for req in pkg.requires:
        providers = installed.filter(provides=req)
        if providers:
            for provider in providers:
                if pkg.name != provider.name and pkg not in graph[provider]:
                    graph[pkg].add(provider)

from graphlib import TopologicalSorter, CycleError
cycles = True
ts = TopologicalSorter(graph)

while cycles:
    try:
        result = [*TopologicalSorter(graph).static_order()]
        cycles = False
    except CycleError as e:
        # Remove a cycle
        graph[e.args[1][1]].remove(e.args[1][0])

import pprint
pprint.pprint([p.name for p in result])
