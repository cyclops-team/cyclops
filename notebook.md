# Notebook for human notes / feedback

- add support for worktrees
- does the install command on the landing page work?
- update landing page to show the number of github starts rather than "Star"


Yahir's feedback
- A good UI for seeing the different session tabs and the different sessions available
- A good way of being able to see the different agent states in the bottom left (optional since the tmux pane have names and show their activity status next to it
- Codex works nicely with the Cyclops system but for some reason, Claude Code’s status doesn’t update when it is being used so it just stays in an unknown state which is not ideal
- The Codex color theme looks weird with the text highlighting which is something to fix
- Messaging is very native for the AI bots to run, all it really takes is a cyclops send command which is super chill. A minimal SKILL.md explaining how it works would be good. I used to have a COORDINATION.md and HIYA.md that explains how to coordinate and work together
- Claude ALLEGEDLY did testing to make sure the status indicator for the agents would update but honestly after seeing it, it doesn’t seem to be true or work fully


- it seems like agents would still need some kind of skill to understand that they need to use a cyclops command to communicate


- cyclops chrome: implementer ● ? unknown
    - should show idle but it was showing unknown instead
- how were the peeking rules in the manifests derived?
- does keeping everything on disk fill up memory super fast?


Notes after testing workspace:

Most things are broken. here are some of the immediate things I notice
- When I click on a pane, the switch is kind of slow. The time that it takes for the border around that pane to highlight, as if I'm selecting that, is slower than herdr.
- Clicking the plus tab button doesn't work.
- The tabs, when I started the workspace, already have a name. One of them is %0, and the other one is %1.
- Clicking the arrow in the left sidebar next to the workspace name doesn't work.
- When I click anywhere within a pane, the cursor appears to move to that spot, so it's not actually fixing itself to the spot where you're typing.
- I can't type into any of the panes.
- When I try dragging, it shows a little back-and-forth arrow symbol, and the drag is delayed.
- Right-click doesn't work. No menu shows up.
- There is no app-level menu.
I can't reopen the workspace by simply typing Cyclops, it gives me the guide with possible options and commands for cyclops


Notes after fixing workspace (1st pass):

Okay, so a few notes. For the workspace, I want the active workspace. I want to signify the active workspace with a contrasting background, either a bright or contrasting background, to show the user which workspace is active. I also want this for the tabs. I want the tab to have a bright or some type of contrasting background, and then the text should be contrasting with that background to indicate which active tab the user is on.

When you click on the plus button to create a new tab, I want a little modal to pop up. The new tab modal has an input field for the name of the tab and then a button to save or to clear or to cancel.

I want there to be a gutter between the pains, almost like a padding between the pains, so that their borders are not co-linear.

I want the split plan buttons to be visible on all plans, not just the active one or the in-focus one.

I want workspaces to be named based off of the current folder or directory that you're in the pane. When I create a new workspace, It should open up a new empty pane, add a tab to the Workspaces bar, and it should take the workspace name of just whatever folder you're in. I don't want a modal to pop up for a new workspace.

For each tab in the workspace sidebar, I should be able to right-click on it and rename or close that workspace.

For each tab in the tabs, I should be able to right-click on it and either rename the tab or close the tabs.

I also want the top section, where it has the label for the workspaces sidebar and where the tabs are, to have some sort of visual separation from the rest of the app where you're actually working. For the sidebar, it could just be a little bit of space under the title "Workspaces". For the tabs bar, maybe there could even be just a background horizontally along that entire section of the workspace.

For any menus, I want a hover effect when you're hovering over an option with your mouse.

Overall, I just want to improve how visually aesthetic and easy to work with this app is, and make it as ergonomic as something like Hurter. I have attached an image as an example.
