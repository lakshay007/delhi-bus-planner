# Technical note

Delhi bus journey planner — Transport Stack Hackathon 2026, Problem 1.

The system rests on one table built from the live GPS feed: **"a bus went from stop A to the next stop B, starting at time t, and took x seconds."** We call a stop-to-next-stop pair a *stretch*; the network has 14,865 of them. Section 1 is how that table is produced, section 2 is how it becomes a travel-time estimate, and section 3 is how live data changes the answer for someone leaving now.

```
GPS feed ─► position along the route ─► time at each stop ─► stretch times
                                                                 │
                                    usual time per stretch ◄─────┤ (all days)
                                    live correction        ◄─────┘ (last hour)
                                              │
                              retimed timetable + tracked buses ─► journey search
```

## 1. How GPS points are linked to routes

**We match each bus to its own route's line, not to a road network.** Every GPS point in the feed carries a `route_id`, and in this GTFS a `route_id` is exactly one direction, one path and one stop sequence. So the question "which road is this bus on" is already answered; what remains is "how far along its route is it". That reduces map matching to one dimension and removes its main failure, snapping to a parallel road.

1. **Prepare each route once.** Measure the distance along the route's path to every stop. On 93 routes (3.6%) the path loops or doubles back, so the nearest point on the path is the wrong pass; stops are placed with an order-preserving search instead (monotone dynamic programming).
2. **Project each GPS point onto the path** (linear referencing). A point further than 60 m from the path is off-route and dropped. Moving buses sit a median of 7 m from their path.
3. **Resolve loops.** Where a point is near two passes of the same route, we choose the sequence of positions that moves forward at plausible speeds (Viterbi, the standard map-matching method, in one dimension).
4. **Split into runs.** A run is one bus making one trip. It ends when the feed's dispatch id or route changes, or the bus is silent for 10 minutes.
5. **Fix wrong direction tags.** If a run makes no sense on its tagged route but is a clean forward run on the opposite direction, it is re-tagged. On 1 Oct this recovered 997 runs.
6. **Remove parked buses.** Buses at depots and terminals report constantly. A run starts when the bus is 25 m past its first stop, so waiting at the terminal never counts as travel.

The feed's own `speed` field is not used (it is wrong by a factor of about three), and its `trip_id` is used only as a run boundary because it does not match the timetable.

## 2. How travel time is estimated on each stretch

### Measuring one bus on one stretch

A bus reports about every 22 seconds, so it is rarely seen exactly at a stop. The time it reached each stop is interpolated between the GPS points on either side. The time for a stretch is the difference between two consecutive stop times.

- **Time at stops, signals and queues is included.** The rider experiences the total, and 22-second sampling cannot separate a stop from a signal next to it. A stop counts as reached 15 m before it, so waiting at a stop belongs to the stretch that starts there.
- **Errors do not accumulate.** Because every stretch runs from one stop time to the next, the stretches of a ride add up exactly to its end-to-end time. Interpolation error at the stops in between cancels.
- **Doubtful measurements are dropped:** stop times interpolated across a GPS gap over 2 minutes, jumps faster than 108 km/h, and stretch speeds outside 1.4–90 km/h. On 1 Oct this kept 1,269,310 measurements and rejected 237,730.

### Predicting the usual time

Most stretches have few measurements at any given half-hour, and 44% are served by a single route. So the estimate is built from broad to specific:

`seconds = length × city pace × stretch factor × time-of-day factor`

All three factors are learned from past days; nothing here is live. For a trip on another day this is the whole estimate. For a trip leaving now it is the starting point that section 3 corrects.

| Factor | Answers | Learned from |
|---|---|---|
| City pace | How slow are Delhi's buses at this half-hour, on this kind of day? | Every bus in the city |
| Stretch factor | Is this stretch slower or faster than the city in general? | All measurements on the stretch |
| Time-of-day factor | Is this stretch worse than its own normal at this half-hour, on this kind of day? | Measurements on the stretch in that half-hour |

Four kinds of day are kept apart: Monday to Friday, Saturday, Sunday, and public holidays. In the archive Saturdays ran like weekdays after noon, Sundays ran faster all day, and the two public holidays ran faster than any Sunday.

Each factor is trusted in proportion to its data (shrinkage with pseudo-counts, in the style of empirical Bayes). With few measurements a factor stays close to the level above it, which gives a fallback order:

1. the stretch's own history at this half-hour and kind of day;
2. the stretch's overall factor with the city's daily rhythm;
3. the factor of adjacent stretches;
4. the city pace alone;
5. the timetable's 18 km/h, only when there is no data at all.

A single measurement can say at most "3 times slower" or "3 times faster" than expected, so one stuck bus cannot dominate. Every bus leg in the API response reports which of these levels its estimate rests on.

The city pace alone shows what the timetable lacks. Across October 2026, weekday buses averaged 31 km/h at 2 am, 20 km/h at 8 am, 16.5 km/h from noon to 4 pm and 13.4 km/h at 6 pm. The timetable assumes 18 km/h at every hour.

The settings (the 3-times limit, how strongly thin data is pulled toward the level above, which kinds of day to separate) were chosen by cross-validation on the archive: hold out a week, fit on the rest, score whole bus rides. A plain average per stretch and half-hour was tried the same way and was less accurate, because single slow buses distort it.

## 3. How live and historical data are combined

History gives the usual time. Live data is used for trips leaving between 15 minutes ago and 4 hours ahead, in two ways: it corrects how fast buses move, and it decides which buses exist. The live view is rebuilt every 30 seconds.

### Correcting the speed

For each stretch we compute a **live ratio**: how long buses took in the last hour, divided by the usual time.

- Recent buses count most; a measurement's weight halves about every 10 minutes.
- Adjacent stretches contribute at half weight.
- With few recent buses the ratio leans toward the city-wide ratio, and that leans toward 1. One slow bus moves it a little; several agreeing move it a lot.

The ratio is then weakened by how far ahead the bus will reach the stretch:

`seconds = usual × ratio ^ fade`, with `fade = exp(−hours ahead / 6)`

For a stretch running 1.3 times slower than usual right now:

| Bus reaches the stretch in | Multiplier applied |
|---|---|
| 0 min | 1.30 |
| 30 min | 1.27 |
| 1 hour | 1.25 |
| 2 hours | 1.21 |
| 4 hours | 1.14 |

The fade is slow because the data says it should be. We replayed the archive with fades from 30 minutes to 16 hours and scored trips leaving now and up to 6 hours later. A stretch that is slower than usual now is mostly still slower hours later, and live data stopped helping only for trips leaving about 6 hours ahead.

### Deciding which bus you catch

The static timetable is right about how often buses run and wrong about exactly when. Across the archive, 255,475 observed terminal departures were a median 4.3 minutes from the nearest timetabled start, barely better than the 5.0 minutes expected if buses ignored the timetable. The amount of service is about right: on 13 Oct we saw 58,117 bus runs against 65,322 timetabled trips. So for trips leaving now, the planner boards real buses.

- **Tracked buses.** Each bus seen in the last 5 minutes, on its route, past its first stop and moving, is run forward from its last GPS position. It still owes the rest of the stretch it is on, then each later stretch at the corrected time. That gives its arrival at every stop ahead. About 3,000 buses on 1,100 routes were tracked at midday on 13 Nov.
- **Passed stops** are unavailable: a bus cannot be boarded behind its current position.
- **Buses standing at the first stop.** A bus seen within 200 m of its route's first stop, not yet under way, is the route's next departure. It is expected to set off a few minutes after it was last seen (2 minutes for a bus that has just arrived, up to 10 for one that has stood longer) and replaces the nearest timetabled start. Replayed against real arrivals, this halves the miss in the waiting time at the stops it affects (9.2 against 19.6 minutes on a day the rule was not fitted on). One such bus in five does not leave, so the response marks it as an estimate, not as tracked.
- **Buses not yet dispatched** come from the timetable's start times, with the same corrected travel times. The feed does not say when a parked bus will leave. For these legs the response adds what the route really ran: how many buses passed the boarding stop in the last hour against the timetable's number.
- **Buses that are not there.** On a route with tracked buses, timetabled trips that should be on the road but are not seen are dropped.

The journey search (RAPTOR, up to three buses, with walking to and between stops) runs on this combined timetable. Each bus leg in the response says whether it is a tracked bus, a bus standing at its first stop (both with the number plate), or a timetabled departure.

### What happens when live data is missing

| Situation | What is used |
|---|---|
| Many buses just crossed the stretch | Their times, almost in full |
| No bus on this stretch lately | Adjacent stretches, then the city-wide ratio |
| No live data at all, or it is over 5 minutes old | Usual times and the timetable's departures |
| The feed has been recorded for under 20 minutes | Tracked buses are used; the speed correction waits, because at first only quick passes have had time to finish |
| Trip leaves more than 4 hours ahead, or on another day | Usual times only |
| Route has no tracked bus | A bus standing at the first stop if there is one, then timetabled departures |

## Limits

- The feed runs about 100 seconds behind reality, so "now" is a minute and a half ago.
- The archive is one month. Seasons, school holidays and festival weeks are not learned; for trips leaving soon the live correction covers them, for trips days ahead nothing does.
- Public holidays come from a short list that has to be kept up to date, and the archive holds only two of them.
- The estimate is the typical ride. It runs about 2% short of the average ride, because the few very slow passes that the 3-times limit trims are real.
- Accuracy against held-out bus trips and against Google Maps is reported in the validation report.
