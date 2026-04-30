Based on the search results, I can clarify the container lifecycle concepts and provide guidance for your devcontainer-cli GUI app, though I should note that "Apple Containers" appears to refer to two different things: Apple's FSKit container states (for filesystem containers) and their new Containerization framework for Linux containers announced at WWDC 2025.

## Container Image vs Container Instance Lifecycle

A **container image** is an immutable, read-only template that serves as a blueprint for containers  [aws.amazon](https://aws.amazon.com/compare/the-difference-between-docker-images-and-containers/). It's stored on disk and doesn't consume CPU or RAM resources—only storage space  [geeksforgeeks](https://www.geeksforgeeks.org/devops/difference-between-docker-image-and-container/). Images are created by building a Dockerfile and remain unchanged once built  [cleanstart](https://www.cleanstart.com/guide/docker-images-vs-container).

A **container instance** is a runnable, live process created from an image  [aws.amazon](https://aws.amazon.com/compare/the-difference-between-docker-images-and-containers/). The lifecycle includes several distinct states  [dev](https://dev.to/docker/docker-architecture-life-cycle-of-docker-containers-and-data-management-1a9c):

1. **Created**: Container exists but hasn't started yet
2. **Running**: Container is actively executing
3. **Paused**: Container execution is suspended (not all runtimes support this)
4. **Stopped**: Container has exited but still exists with its state preserved
5. **Removed/Deleted**: Container is completely destroyed

The key difference: a **stopped container** still maintains a reference to its originating image and retains its runtime state, file system changes (in the writable layer), and configuration  [codesignal](https://codesignal.com/learn/courses/diving-deeper-into-images-containers/lessons/deleting-images-and-containers). This is why you cannot delete an image while containers—even stopped ones—still depend on it  [reddit](https://www.reddit.com/r/docker/comments/15vhz00/why_cant_we_remove_images_after_starting/). The image remains "mounted" as the base filesystem for the container  [reddit](https://www.reddit.com/r/docker/comments/15vhz00/why_cant_we_remove_images_after_starting/).

## Update Logic for devcontainer.json and Dockerfile Changes

For a devcontainer-cli based GUI app, the correct update logic depends on what changed  [stevekinney](https://stevekinney.com/courses/visual-studio-code/working-with-dev-containers):

### When to Rebuild the Container

**Dockerfile changes** always require a rebuild  [stevekinney](https://stevekinney.com/courses/visual-studio-code/working-with-dev-containers):
- Adding/updating software packages
- Changing base image versions
- Modifying build-time arguments
- Installing dependencies

**devcontainer.json changes** that require rebuild  [stevekinney](https://stevekinney.com/courses/visual-studio-code/working-with-dev-containers):
- Switching to a different `image` or `dockerFile` reference
- Changing `build` section parameters

**devcontainer.json changes** that may not require rebuild  [stevekinney](https://stevekinney.com/courses/visual-studio-code/working-with-dev-containers):
- Adding VS Code extensions (these can be applied without rebuild in some cases)
- Modifying settings, environment variables, or port forwarding
- Changing lifecycle commands like `postCreateCommand`

### Recommended Workflow Using devcontainer-cli

The devcontainer CLI provides specific commands for this workflow  [github](https://github.com/devcontainers/cli):

1. **For Dockerfile changes**: Use `devcontainer build` to rebuild the image, then `devcontainer up` to recreate the container  [github](https://github.com/devcontainers/cli)
2. **For configuration changes**: Run `devcontainer up` which will detect changes and rebuild if necessary  [github](https://github.com/devcontainers/cli)
3. **To force a complete rebuild**: Use `devcontainer down` (stops and deletes containers) followed by `devcontainer build --no-cache` and `devcontainer up`  [github](https://github.com/devcontainers/cli)

### Critical Implementation Notes

For your GUI app, you should implement logic that:

- **Detects configuration changes** by comparing the current devcontainer.json/Dockerfile with what was used to build the running container
- **Stops and removes the old container** before creating a new one when images change (containers are disposable)  [cleanstart](https://www.cleanstart.com/guide/docker-images-vs-container)
- **Uses `--no-cache` flag** when base images need updating but configuration files haven't changed  [docs.nav2](https://docs.nav2.org/development_guides/devcontainer_docs/devcontainer_guide.html)
- **Avoids using hardcoded `image` references** in devcontainer.json if you want rebuild functionality to work properly—use `dockerFile` instead  [stackoverflow](https://stackoverflow.com/questions/75780051/where-did-the-vscode-rebuild-container-command-go)

The modern container philosophy treats containers as **ephemeral**: when updates are needed, you rebuild a new image version and replace containers, rather than trying to preserve long-lived container instances  [cleanstart](https://www.cleanstart.com/guide/docker-images-vs-container).
