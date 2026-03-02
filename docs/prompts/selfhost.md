❯ Now I want to make Warcraft completely self-hosting from user perspective. I need two ways
  to interact with the system. One will be to enter a prompt to a kind of coordinator agent,
  which ideally would have some context of what was going on. These prompts end up being
  quite complex probably, so the feedback on this isn't quick. be cool to have a running
  cloud code session we can actually work with. We can do interactive prompting with and just
   be sending the messages from, yeah, or an amplifier set up, right? And just be sending
  messages to it. Now, I can't use the polling loop we're talking about. It's actually going
  to wake up instantly when you send something. And then this agent itself is kind of like a
  hidden task in the system. it's sort of the mayor from Gastown. Don't call it that though.
  Probably best to call it the coordinator. Yeah, so it's going to be helping us to place
  things in the work graph. And then another change I want to make is I want to be able to
  use the TUI to add to message, to add nodes to the graph, to edit dependencies, to do
  everything basically. I want a kind of like tracker type system based on as few commands
  incantations as possible, mostly verbally driven via interface with this long running
  agent, the coordinator. Okay.  Yeah, and this also means a lot of stuff like we need the
  message passing system to be working and tested. We need a way, obviously, to communicate
  with the coordinating LLM or agent, active agent, or whatever the hell it's called. So the
  agent's got to be running and we're going to be feeding it stuff. It's a really huge, huge
  thing, I think, but if we can get it right, Warcraft lifts off the ground and flies
  forever. The 2Wi is compelling stuff, but right now my whole 2Wi is actually a cloud code
  window on top and a 2Wi on bottom. I should have a long running chat log with whatever I
  want, like basically a terminal inside the 2Wi. It could be a terminal. It's a bit nasty to
   support that. Probably a much simpler kind of interface, like just message and then see
  the output. I'll just be brutal and get the formatting right. I don't know, there's
  something too actually, just running directly a terminal. Let's say that I don't know
  exactly how it will work out, but I do think that it's not easy for users to understand how
   to use it because you can't say, oh, run this command, and then you get this UI. And yeah,
   you just work in there. You have to say, oh, yeah, you have to tell the top-level agent
  about it, and it has to make sure it's doing it, and so on and so on. and it's all a bit
  like hmm and part of the problem is top level agent is cloud code and it's going to be
  trying to like hijack the system and do its own stuff all the time So maybe there's really
  a level of just, you know, efficient use of the system is to send like, POMs to it, that
  are really specific. They just say, hey, you're in this world, you can do these things, you
   can run these commands. Now that's the one interface that's probably not supported by raw
  endpoints. We have to have a full executor that... I don't know, I wonder if we could use
  Amplifier or... or just re-implement Amplifier. Honestly, I don't think it's that
  complicated. I mean, I have to exaggerate a little bit, but I don't think it's that
  complicated to get right. At least to the level we need it to work. I don't want to have
  external dependencies on some Python stuff. So, maybe this is actually a project to
  bootstrap, which is kind of like our version of the amplifier, or we find a Rust version of
   the amplifier that we can build on top of. I just think something has bundles in it, the
  bundling concept has to be the same, I want to use their bundles, because that's not a bad
  way of working with it. This is a massive thing, I'm describing. And so I think you really
  appreciate what I mean when I say we're going to build now. After we've committed and
  everything is stable, by the way, so you have to have an initial task before you implement
  this particular sub-task, or you can implement it and pause it. That's also a safe thing to
   do. Will be to make the autopoietic organization implement this. So you're going to be
  taking this prompt and rendering a top-level system prompt for an agent that will research,
   spawn off a lot of dependents, potentially connect them back to other kinds of validation
  phases, and build a whole work plan for this. And then, of course, all agents seem to be
  reminded, this is really crucial because we haven't really implemented this properly in, I
  don't think so in the quick start, but the agents should really remember that they are
  expected to add tasks to the graph that they're part of as they find them, as they discover
   new things. We need to be sure that they remember that the graph is a kind of medium that
  they're able to operate on. Stigmergy. Yeah, so let's not get distracted. You're going to
  first spawn an initial agent to just make sure that we checkpoint the stable repository,
  everything is stable. Things are going to be modular enough. I don't think we have to use
  work trees for this, but it could be something that makes sense to do. An agent should be
  encouraged to do it if they do, and we should also have a process and a validation take
  stock of what work trees have been created, what kind of diff we have, all this kind of
  stuff. a complete validation phase to this that is multi-cycle potentially. Does this all
  make sense to you? Set the spark, light the fire.
