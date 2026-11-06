# Reef into Elasticsearch, through Vector

Every prompt, reply, tool call and finished turn in the fleet, searchable in Kibana. Reef starts
[Vector](https://vector.dev) as an `exec` sink in `plain` mode and writes records to its stdin;
Vector bulk-writes them into a daily `reef-fleet-*` index. Recording is described in
[docs/reef/record.md](../../../../docs/reef/record.md).

This needs Docker. It runs Elasticsearch with security off on localhost only: for trying, not
for keeping anything.

1. Start Elasticsearch and Kibana:

   ```sh
   docker compose up -d
   ```

2. Copy `record.json` into Reef's data dir (`reef-data/` beside `hosts.json`), and change
   `/path/to/vector-elasticsearch` to where this folder is. Reef picks it up within a second; no
   restart. To try it with no fleet, the demo records too:

   ```sh
   reef --demo --data-dir ./demo-data    # with record.json in ./demo-data
   ```

3. Wait for an agent to finish a turn (the demo's do every half minute), then count what landed:

   ```sh
   curl -s 'localhost:9200/reef-fleet-*/_count'
   ```

4. In Kibana, http://localhost:5601, add a data view for `reef-fleet-*` with `time` as its time
   field, and open Discover.

A reply that streamed, or a tool call that finished, is written again under the same id, so the
index holds the latest of each. To stop: take the sink out of `record.json` (Reef stops Vector),
then `docker compose down -v`.
